import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.registerTool({
        name: "greet",
        description: "Greets a user by name",
        parameters: {
            type: "object",
            properties: {
                name: {
                    type: "string",
                    description: "The name to greet"
                }
            },
            required: ["name"]
        },
        execute: async (args: any) => {
            return {
                content: [{ type: "text", text: `Hello, ${args.name}!` }]
            };
        }
    });
}
