import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.registerCommand("ping", {
        description: "Responds with pong",
        handler: async () => {
            return {
                output: "pong",
                continueLoop: false
            };
        }
    });
}
