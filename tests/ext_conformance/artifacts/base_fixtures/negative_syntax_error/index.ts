// Intentional syntax error - missing closing brace
import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.registerTool({
        name: "broken",
        description: "This will not parse"
    // Missing closing braces
