import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    // Tool registration
    pi.registerTool({
        name: "echo",
        description: "Echoes back the input",
        parameters: {
            type: "object",
            properties: {
                text: { type: "string", description: "Text to echo" }
            },
            required: ["text"]
        },
        execute: async (args: any) => {
            return {
                content: [{ type: "text", text: args.text }]
            };
        }
    });

    // Event hook registration
    pi.on("agent_start", async () => {
        // Multi-type extension: tool + event hook
    });
}
