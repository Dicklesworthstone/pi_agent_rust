import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    // Register a tool with missing required fields
    pi.registerTool({
        // Missing 'name' - should fail or produce empty registration
        description: "Tool with no name",
        parameters: { type: "object", properties: {} },
        execute: async () => ({ content: [{ type: "text", text: "unreachable" }] })
    } as any);
}
