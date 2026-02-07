import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.on("agent_start", async () => {
        // Just log for now, or maybe use a session op to prove it ran?
        // But for minimal fixture, registration is enough.
    });
}
