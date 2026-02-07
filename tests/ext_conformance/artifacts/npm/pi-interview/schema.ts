export interface CodeBlock {
	code: string;
	lang?: string;
	file?: string;
	lines?: string;
	highlights?: number[];
	title?: string;
}

export interface RichOption {
	label: string;
	code?: CodeBlock;
}

export type OptionValue = string | RichOption;

export interface Question {
	id: string;
	type: "single" | "multi" | "text" | "image";
	question: string;
	options?: OptionValue[];
	recommended?: string | string[];
	context?: string;
	codeBlock?: CodeBlock;
}

export interface QuestionsFile {
	title?: string;
	description?: string;
	questions: Question[];
}

export function getOptionLabel(option: OptionValue): string {
	return typeof option === "string" ? option : option.label;
}

export function isRichOption(option: OptionValue): option is RichOption {
	return typeof option === "object" && option !== null && "label" in option;
}

const SCHEMA_EXAMPLE = `Expected format:
{
  "title": "Optional Title",
  "questions": [
    { "id": "q1", "type": "single", "question": "Pick one?", "options": ["A", "B"] },
    { "id": "q2", "type": "multi", "question": "Pick many?", "options": ["X", "Y", "Z"] },
    { "id": "q3", "type": "text", "question": "Describe?" },
    { "id": "q4", "type": "image", "question": "Upload?" }
  ]
}
Valid types: single, multi, text, image
Options: array of strings or objects with { label, code? }`;

function validateCodeBlock(block: unknown, context: string): CodeBlock {
	if (!block || typeof block !== "object") {
		throw new Error(`${context}: codeBlock must be an object`);
	}
	const b = block as Record<string, unknown>;
	if (typeof b.code !== "string") {
		throw new Error(`${context}: codeBlock.code must be a string`);
	}
	if (b.lang !== undefined && typeof b.lang !== "string") {
		throw new Error(`${context}: codeBlock.lang must be a string`);
	}
	if (b.file !== undefined && typeof b.file !== "string") {
		throw new Error(`${context}: codeBlock.file must be a string`);
	}
	if (b.lines !== undefined && typeof b.lines !== "string") {
		throw new Error(`${context}: codeBlock.lines must be a string`);
	}
	if (b.title !== undefined && typeof b.title !== "string") {
		throw new Error(`${context}: codeBlock.title must be a string`);
	}
	if (b.highlights !== undefined) {
		if (!Array.isArray(b.highlights) || b.highlights.some((h) => typeof h !== "number")) {
			throw new Error(`${context}: codeBlock.highlights must be an array of numbers`);
		}
	}
	return b as unknown as CodeBlock;
}

function validateOption(option: unknown, questionId: string, index: number): OptionValue {
	if (typeof option === "string") {
		return option;
	}
	if (option && typeof option === "object") {
		const o = option as Record<string, unknown>;
		if (typeof o.label !== "string") {
			throw new Error(
				`Question "${questionId}": option at index ${index} must have a "label" string`
			);
		}
		if (o.code !== undefined) {
			validateCodeBlock(o.code, `Question "${questionId}" option "${o.label}"`);
		}
		return option as RichOption;
	}
	throw new Error(
		`Question "${questionId}": option at index ${index} must be a string or object with label`
	);
}

function validateBasicStructure(data: unknown): QuestionsFile {
	if (Array.isArray(data)) {
		throw new Error(
			`Invalid questions file: root must be an object, not an array.\n\n${SCHEMA_EXAMPLE}`
		);
	}

	if (!data || typeof data !== "object") {
		throw new Error(`Invalid questions file: must be an object.\n\n${SCHEMA_EXAMPLE}`);
	}
	
	const obj = data as Record<string, unknown>;

	if (("label" in obj || "description" in obj) && !("questions" in obj)) {
		throw new Error(
			`Invalid questions file: missing "questions" array. Did you mean to wrap your questions?\n\n${SCHEMA_EXAMPLE}`
		);
	}
	
	if (obj.title !== undefined && typeof obj.title !== "string") {
		throw new Error("Invalid questions file: title must be a string");
	}
	
	if (obj.description !== undefined && typeof obj.description !== "string") {
		throw new Error("Invalid questions file: description must be a string");
	}
	
	if (!Array.isArray(obj.questions) || obj.questions.length === 0) {
		throw new Error(
			`Invalid questions file: "questions" must be a non-empty array.\n\n${SCHEMA_EXAMPLE}`
		);
	}
	
	const validTypes = ["single", "multi", "text", "image"];
	for (let i = 0; i < obj.questions.length; i++) {
		const q = obj.questions[i] as Record<string, unknown>;
		if (!q || typeof q !== "object") {
			throw new Error(`Invalid question at index ${i}: must be an object`);
		}
		if (typeof q.id !== "string") {
			throw new Error(`Invalid question at index ${i}: id must be a string`);
		}

		if (typeof q.type !== "string" || !validTypes.includes(q.type)) {
			const hint = q.type === "select" ? ' (use "single" instead of "select")' : "";
			throw new Error(
				`Question "${q.id}": type must be one of: ${validTypes.join(", ")}${hint}`
			);
		}

		if (typeof q.question !== "string") {
			const hint = "label" in q || "description" in q 
				? ' (use "question" field, not "label" or "description")'
				: "";
			throw new Error(`Question "${q.id}": "question" field must be a string${hint}`);
		}

		if (q.options !== undefined) {
			if (!Array.isArray(q.options) || q.options.length === 0) {
				throw new Error(`Question "${q.id}": options must be a non-empty array`);
			}
			for (let j = 0; j < q.options.length; j++) {
				validateOption(q.options[j], q.id as string, j);
			}
		}

		if (q.context !== undefined && typeof q.context !== "string") {
			throw new Error(`Question "${q.id}": context must be a string`);
		}

		if (q.codeBlock !== undefined) {
			validateCodeBlock(q.codeBlock, `Question "${q.id}"`);
		}
	}
	
	return obj as unknown as QuestionsFile;
}

export function validateQuestions(data: unknown): QuestionsFile {
	const parsed = validateBasicStructure(data);

	const ids = new Set<string>();
	for (const q of parsed.questions) {
		if (ids.has(q.id)) {
			throw new Error(`Duplicate question id: "${q.id}"`);
		}
		ids.add(q.id);
	}

	for (const q of parsed.questions) {
		if (q.type === "single" || q.type === "multi") {
			if (!q.options || q.options.length === 0) {
				throw new Error(`Question "${q.id}": options required for type "${q.type}"`);
			}
		} else if (q.type === "text" || q.type === "image") {
			if (q.options) {
				throw new Error(`Question "${q.id}": options not allowed for type "${q.type}"`);
			}
		}

		if (q.recommended !== undefined) {
			if (q.type === "text" || q.type === "image") {
				throw new Error(`Question "${q.id}": recommended not allowed for type "${q.type}"`);
			}

			const optionLabels = q.options?.map(getOptionLabel) ?? [];

			if (q.type === "single") {
				if (typeof q.recommended !== "string") {
					throw new Error(`Question "${q.id}": recommended must be string for single-select`);
				}
				if (!optionLabels.includes(q.recommended)) {
					throw new Error(
						`Question "${q.id}": recommended "${q.recommended}" not in options`
					);
				}
			}

			if (q.type === "multi") {
				const recs = Array.isArray(q.recommended) ? q.recommended : [q.recommended];
				for (const rec of recs) {
					if (!optionLabels.includes(rec)) {
						throw new Error(`Question "${q.id}": recommended "${rec}" not in options`);
					}
				}
				if (!Array.isArray(q.recommended)) {
					q.recommended = recs;
				}
			}
		}
	}

	return parsed;
}
