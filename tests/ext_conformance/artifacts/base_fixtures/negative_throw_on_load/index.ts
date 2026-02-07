import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    throw new Error("Extension intentionally crashes during initialization");
}
