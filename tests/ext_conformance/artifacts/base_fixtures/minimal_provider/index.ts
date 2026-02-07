import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";

export default function(pi: ExtensionAPI) {
    pi.registerProvider("mock-provider", {
        api: "openai-responses", // Using a known supported API type from Rust
        baseUrl: "https://example.com/v1",
        apiKey: "env:MOCK_KEY",
        models: [
            {
                id: "mock-model",
                name: "Mock Model",
                contextWindow: 4096,
                cost: { input: 0, output: 0 }
            }
        ]
    });
}
