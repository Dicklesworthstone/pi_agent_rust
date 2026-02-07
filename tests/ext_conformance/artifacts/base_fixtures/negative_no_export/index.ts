// Extension without export default - should fail to load
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

function setup(pi: ExtensionAPI) {
    pi.registerTool({
        name: "orphan",
        description: "Never registered because no export default",
        parameters: { type: "object", properties: {} },
        execute: async () => ({ content: [{ type: "text", text: "unreachable" }] })
    });
}
