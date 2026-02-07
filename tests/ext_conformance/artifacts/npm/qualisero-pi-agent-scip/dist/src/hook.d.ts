interface HookContextLite {
    cwd: string;
    tools?: {
        name?: string;
    }[];
    addSystemMessage(message: string): void;
}
export type HookFactory = (pi: {
    on(event: 'session_start', handler: (event: unknown, ctx: HookContextLite) => void | Promise<void>): void;
}) => void;
declare const factory: HookFactory;
export default factory;
//# sourceMappingURL=hook.d.ts.map