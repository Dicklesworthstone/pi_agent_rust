import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    // Register a tool without an execute handler
    pi.registerTool({
        name: "no-handler",
        description: "Tool registered without execute function",
        parameters: { type: "object", properties: {} }
        // Missing: execute handler
    } as any);
}
