import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    // Using hypothetical native API or known pattern
    // The mcp-adapter extension usually handles this by reading config,
    // but if we support it natively:
    if ((pi as any).registerMcpServer) {
        (pi as any).registerMcpServer("test-mcp", {
            command: "echo",
            args: ["hello"]
        });
    }
}
