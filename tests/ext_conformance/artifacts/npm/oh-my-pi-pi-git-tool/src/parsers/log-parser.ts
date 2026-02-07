import type { Commit } from "../types";

const RECORD_SEPARATOR = "\x1e";
const FIELD_SEPARATOR = "\x00";

export function parseLog(output: string): Commit[] {
	const records = output.split(RECORD_SEPARATOR).filter((record) => record.trim().length > 0);
	const commits: Commit[] = [];

	for (const record of records) {
		const fields = record.split(FIELD_SEPARATOR);
		if (fields.length < 11) continue;

		const [
			sha,
			shortSha,
			authorName,
			authorEmail,
			authorDate,
			committerName,
			committerEmail,
			committerDate,
			parentsRaw,
			subject,
			body,
		] = fields;

		const message = body ? `${subject}\n\n${body}` : subject;
		const parents = parentsRaw ? parentsRaw.split(" ").filter(Boolean) : [];

		commits.push({
			sha,
			shortSha,
			author: { name: authorName, email: authorEmail, date: authorDate },
			committer: { name: committerName, email: committerEmail, date: committerDate },
			message,
			subject,
			parents,
		});
	}

	return commits;
}
