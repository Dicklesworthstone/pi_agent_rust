#!/usr/bin/env python3
"""Verify published signed bytes and optional offline CLI behavior; retain evidence."""

import argparse
import hashlib
import hmac
import json
import os
from pathlib import Path
import platform
import re
import ssl
import subprocess
import urllib.parse
import urllib.request

REPO = "Dicklesworthstone/pi_agent_rust"
PUBLIC_KEY_SHA256 = "262444717bc7a49d8e8f7ee34b01b5b6a87734a6d168b008f481fa47c35c882a"
USER_AGENT = "OpenAI File Downloader, XaiImageApiFetch/1.0"
HOSTS = {"github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def save(path, data):
    with path.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def parse_json(data, label):
    try:
        return json.loads(data)
    except (json.JSONDecodeError, UnicodeDecodeError) as error:
        raise ValueError("malformed JSON: " + label) from error


def check_url(url):
    parsed = urllib.parse.urlsplit(url)
    require(parsed.scheme == "https" and parsed.hostname in HOSTS
            and parsed.port in (None, 443) and not parsed.username and not parsed.password,
            "unexpected asset transport URL")


class CheckedRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, file, code, message, headers, newurl):
        check_url(newurl)
        return super().redirect_request(request, file, code, message, headers, newurl)


def metadata(path, output, label):
    result = subprocess.run(
        ["gh", "api", "--hostname", "github.com", "-H", "User-Agent: " + USER_AGENT,
         "repos/" + REPO + path], capture_output=True, timeout=60, check=True,
    )
    save(output / (label + ".json"), result.stdout)
    return parse_json(result.stdout, label)


def checksum_rows(data):
    rows = {}
    for line in data.decode("ascii").splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([0-9a-fA-F]{64})\s+\*?([A-Za-z0-9][A-Za-z0-9_.-]*)", line)
        require(match is not None, "malformed checksum row")
        name = match[2]
        require(name not in rows, "duplicate checksum filename")
        rows[name] = match[1].lower()
    require(bool(rows), "empty checksum manifest")
    return rows


def release_snapshot(release):
    snapshot = {key: release.get(key) for key in
                ("id", "tag_name", "name", "body", "draft", "prerelease", "published_at")}
    snapshot["assets"] = sorted((row["id"], row["name"], row["size"], row["state"],
                                 row.get("digest"), row["browser_download_url"]) for row in release["assets"])
    return snapshot


def verify(options, output, receipt):
    require(not any(os.environ.get(key) for key in
                    ("SSL_CERT_FILE", "SSL_CERT_DIR", "PYTHONHTTPSVERIFY", "CURL_CA_BUNDLE")),
            "ambient TLS override forbidden")
    public_key = Path(__file__).resolve().parent.parent / "minisign.pub"
    require(hmac.compare_digest(digest(public_key.read_bytes()), PUBLIC_KEY_SHA256), "trusted public key changed")
    obj = metadata("/git/ref/tags/" + options.tag, output, "tag-ref")["object"]
    for depth in range(5):
        if obj["type"] != "tag":
            break
        obj = metadata("/git/tags/" + obj["sha"], output, "tag-" + str(depth))["object"]
    require(obj["type"] == "commit" and obj["sha"] == options.commit, "peeled tag source mismatch")
    before = metadata("/releases/tags/" + options.tag, output, "release-before")
    require(before["tag_name"] == options.tag and before["published_at"]
            and not before["draft"] and not before["prerelease"], "release is not publicly published")
    assets = {row["name"]: row for row in before["assets"]}
    require(len(assets) == len(before["assets"]), "duplicate asset filename")
    require(len({row["id"] for row in assets.values()}) == len(assets)
            and all(row["state"] == "uploaded" and row["size"] > 0 for row in assets.values()),
            "incomplete or duplicate asset identity")
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}), CheckedRedirect(),
        urllib.request.HTTPSHandler(context=ssl.create_default_context()),
    )

    def download(name):
        require(name in assets, "missing asset: " + name)
        row = assets[name]
        url = "https://github.com/" + REPO + "/releases/download/" + options.tag + "/" + name
        require(row["browser_download_url"] == url, "asset download route changed")
        check_url(url)
        require(row["size"] <= 256 * 1024 * 1024, "asset exceeds bounded download size")
        with opener.open(urllib.request.Request(url, headers={"User-Agent": USER_AGENT}), timeout=60) as response:
            check_url(response.url)
            data = response.read(row["size"] + 1)
        require(len(data) == row["size"], "asset size mismatch: " + name)
        require(not row.get("digest") or hmac.compare_digest(row["digest"], "sha256:" + digest(data)),
                "GitHub asset digest mismatch: " + name)
        save(output / name, data)
        return data

    def signature(name):
        subprocess.run(["minisign", "-Vm", str(output / name), "-x", str(output / (name + ".minisig")),
                        "-p", str(public_key)], capture_output=True, timeout=60, check=True)

    manifest = download("SHA256SUMS")
    download("SHA256SUMS.minisig")
    signature("SHA256SUMS")
    rows = checksum_rows(manifest)
    expected = set(rows) | {name + suffix for name in rows for suffix in (".sha256", ".minisig")}
    expected |= {"SHA256SUMS", "SHA256SUMS.minisig"}
    require(set(assets) == expected, "published asset inventory differs from signed manifest contract")
    for name in options.asset:
        require(name in rows, "requested asset is not a signed payload")
        data = download(name)
        require(hmac.compare_digest(digest(data), rows[name]), "signed payload checksum mismatch: " + name)
        sidecar = download(name + ".sha256")
        text = sidecar.decode("ascii").strip()
        require((text.lower() == rows[name] if re.fullmatch(r"[0-9a-fA-F]{64}", text)
                 else checksum_rows(sidecar) == {name: rows[name]}), "payload sidecar mismatch")
        download(name + ".minisig")
        signature(name)
        receipt["payloads"][name] = rows[name]
    after = metadata("/releases/tags/" + options.tag, output, "release-after")
    require(release_snapshot(before) == release_snapshot(after), "release changed during verification")
    obj = metadata("/git/ref/tags/" + options.tag, output, "tag-ref-after")["object"]
    for depth in range(5):
        if obj["type"] != "tag":
            break
        obj = metadata("/git/tags/" + obj["sha"], output, "tag-after-" + str(depth))["object"]
    require(obj["type"] == "commit" and obj["sha"] == options.commit, "peeled tag changed during verification")
    receipt.update(release_id=before["id"], asset_count=len(assets), signed_manifest_rows=len(rows),
                   public_key_sha256=PUBLIC_KEY_SHA256)
    if options.smoke:
        native = {("Darwin", "arm64"): "pi_darwin_arm64", ("Darwin", "x86_64"): "pi_darwin_amd64",
                  ("Linux", "x86_64"): "pi_linux_amd64", ("Linux", "aarch64"): "pi_linux_arm64",
                  ("Windows", "AMD64"): "pi_windows_amd64.exe"}.get((platform.system(), platform.machine()))
        require(native in options.asset, "--smoke requires the raw native signed payload")
        binary = output / native
        binary.chmod(0o700)
        for directory in ("home", "config", "cache", "data", "state", "tmp", "project"):
            (output / directory).mkdir(mode=0o700)
        env = {"PATH": os.defpath, "HOME": str(output / "home"), "TMPDIR": str(output / "tmp"),
               "XDG_CONFIG_HOME": str(output / "config"), "XDG_CACHE_HOME": str(output / "cache"),
               "XDG_DATA_HOME": str(output / "data"), "XDG_STATE_HOME": str(output / "state"),
               "PI_CODING_AGENT_DIR": str(output / "home/.pi/agent"), "NO_COLOR": "1", "LANG": "C.UTF-8"}
        for label, arguments in [("version", ["--version"]), ("providers", ["--list-providers"]),
                                 ("policy", ["--extension-policy", "safe", "--explain-extension-policy"])]:
            result = subprocess.run([str(binary), *arguments], env=env, cwd=output / "project",
                                    capture_output=True, text=True, timeout=60, check=False)
            save(output / (label + ".stdout"), result.stdout.encode())
            save(output / (label + ".stderr"), result.stderr.encode())
            receipt["smoke_commands"].append({"arguments": arguments, "exit": result.returncode})
            require(result.returncode == 0, "offline smoke command failed: " + label)
            if label == "version":
                require(re.search(r"^pi " + re.escape(options.tag[1:]) + r"\b", result.stdout), "CLI version mismatch")
            elif label == "providers":
                require(all(name in result.stdout for name in ("anthropic", "openai", "ollama")), "provider catalog incomplete")
            else:
                policy = parse_json(result.stdout, "safe extension policy")
                require(policy["effective_profile"] == "safe" and isinstance(policy["allow_dangerous"], bool)
                        and not policy["allow_dangerous"], "safe policy mismatch")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True, help="expected full peeled release source SHA")
    parser.add_argument("--asset", action="append", required=True, help="signed payload; repeat for more platforms")
    parser.add_argument("--output", type=Path, required=True, help="new retained evidence directory")
    parser.add_argument("--smoke", action="store_true", help="native version, offline providers and safe policy only")
    options = parser.parse_args()
    require(re.fullmatch(r"v\d+\.\d+\.\d+", options.tag), "expected stable vX.Y.Z tag")
    require(re.fullmatch(r"[0-9a-f]{40}", options.commit), "expected full commit SHA")
    require(len(options.asset) == len(set(options.asset)), "duplicate requested asset")
    output = options.output.absolute()
    output.mkdir(mode=0o700)
    output = output.resolve()
    receipt = {"repository": REPO, "tag": options.tag, "source_sha": options.commit,
               "payloads": {}, "smoke_commands": [], "status": "FAIL",
               "scope": "selected published signed bytes; optional offline CLI; no installer, updater, inference or DSR quality claim"}
    try:
        verify(options, output, receipt)
        receipt["status"] = "PASS"
    finally:
        save(output / "receipt.json", (json.dumps(receipt, indent=2) + "\n").encode())
    print(json.dumps(receipt))


if __name__ == "__main__":
    main()
