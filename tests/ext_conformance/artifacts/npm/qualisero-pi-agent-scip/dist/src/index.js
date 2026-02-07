import { createScipTools } from './tools.js';
const factory = async (pi) => {
    const toolsOrPromise = createScipTools(pi);
    const tools = (toolsOrPromise instanceof Promise
        ? await toolsOrPromise
        : toolsOrPromise);
    return tools;
};
export default factory;
//# sourceMappingURL=index.js.map