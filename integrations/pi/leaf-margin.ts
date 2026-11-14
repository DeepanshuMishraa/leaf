/**
 * leaf-margin: hand plans and changes to the person for review in leaf.
 *
 * Installed by `leaf hook install`; `leaf hook uninstall` removes it.
 * Run `leaf hook install` again to refresh it; edit a copy rather than this file.
 */

import { Type } from "@earendil-works/pi-ai";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";

const LEAF = "__LEAF__";

/** Runs leaf until the person sends the review, and hands it to the model. */
async function review(pi: ExtensionAPI, args: string[], signal?: AbortSignal) {
	const result = await pi.exec(LEAF, [...args, "--wait"], { signal });
	const text = (result.stdout || result.stderr || "").trim();
	return {
		content: [{ type: "text" as const, text: text || "leaf returned no review." }],
		details: { code: result.code },
		isError: result.code !== 0,
	};
}

const submitPlan = (pi: ExtensionAPI) =>
	defineTool({
		name: "leaf_review_plan",
		label: "Review plan in leaf",
		description:
			"Show a Markdown plan file to the person for review in leaf, and wait for their verdict. " +
			"Write the plan to a file first. The result is their review: approved, approved with notes, " +
			"or changes requested with numbered items to address. Don't start implementing a plan " +
			"until it's approved.",
		parameters: Type.Object({
			path: Type.String({ description: "Path to the Markdown plan file" }),
		}),
		async execute(_id, params, signal) {
			return review(pi, ["plan", params.path], signal);
		},
	});

const splitArgs = (text: string): string[] => {
	const out: string[] = [];
	let current = "";
	let quote: '"' | "'" | null = null;
	for (const ch of text) {
		if (quote) {
			if (ch === quote) quote = null;
			else current += ch;
		} else if (ch === '"' || ch === "'") {
			quote = ch;
		} else if (/\s/.test(ch)) {
			if (current) {
				out.push(current);
				current = "";
			}
		} else {
			current += ch;
		}
	}
	if (current) out.push(current);
	return out;
};

const requestReview = (pi: ExtensionAPI) =>
	defineTool({
		name: "leaf_review_changes",
		label: "Review changes in leaf",
		description:
			"Show your code changes to the person for review in leaf, file by file, and wait for their " +
			"verdict. The result is their review: approved, approved with notes, or changes requested " +
			"with numbered items grouped by file.",
		parameters: Type.Object({
			source: Type.Optional(
				Type.String({
					description:
						"What to review: empty for unstaged changes, or --staged, --branch, a range such as main...HEAD, or a pull request link",
				}),
			),
		}),
		async execute(_id, params, signal) {
			const source = splitArgs(params.source ?? "");
			return review(pi, ["review", ...source], signal);
		},
	});

export default function (pi: ExtensionAPI) {
	pi.registerTool(submitPlan(pi));
	pi.registerTool(requestReview(pi));
}
