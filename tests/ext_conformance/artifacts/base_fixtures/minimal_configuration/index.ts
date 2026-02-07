import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.registerFlag({
        name: "verbose",
        description: "Enable verbose output",
        type: "boolean",
        default: false
    });

    pi.registerShortcut({
        key: "ctrl+shift+v",
        description: "Toggle verbose mode",
        action: async () => {
            // Toggle verbose flag
        }
    });
}
