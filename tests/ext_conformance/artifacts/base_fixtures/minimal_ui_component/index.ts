import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.registerMessageRenderer({
        contentType: "test/plain",
        render: (content: any) => {
            return `[rendered] ${content.text || ""}`;
        }
    });
}
