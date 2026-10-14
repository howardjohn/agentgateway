import { Fragment, type ReactNode, useState } from 'react';

import { StatusBanner, YamlBlock } from '@/components/Primitives';
import { SchemaHelpPanel } from '@/components/SchemaHelpPanel';
import { mergeEdits } from '@/mergeEdits';
import type { SchemaHelp } from '@/schemaHelp';

export function UnsupportedYamlFallback(props: {
	title: string;
	children: ReactNode;
	value: unknown;
	schema?: unknown;
	help?: SchemaHelp;
}) {
	return (
		<div className="policy-editor-stack">
			<StatusBanner state="warn" title={props.title}>
				{props.children}
			</StatusBanner>
			{props.schema && props.help ? (
				<SchemaHelpPanel schema={props.schema} help={props.help} />
			) : null}
			<YamlBlock value={props.value} />
		</div>
	);
}

// Every structured editor saves through this: `edited` is the value rebuilt
// from form state, and the first one seen is the editor's baseline. Returns the
// edit applied onto `original` (see mergeEdits) and any paths it would replace.
// Pass `edited` before any cleanup that drops modeled fields (e.g. implied
// defaults) and apply that cleanup to the result, or a dropped field looks
// unmodeled and the original's value is kept.
export function useMergedEdits<T>(original: unknown, edited: T): { value: T; lost: string[] } {
	const [baseline] = useState(edited);
	if (original === null || original === undefined) return { value: edited, lost: [] };
	const merged = mergeEdits(original, baseline, edited);
	return { value: merged.value as T, lost: merged.lost };
}

export function LostFieldsWarning(props: { lost: string[] }) {
	if (!props.lost.length) return null;
	const paths = props.lost.filter(path => path !== '.');
	return (
		<StatusBanner state="warn" title="This form cannot show all of this configuration">
			Saving will overwrite{' '}
			{paths.length === props.lost.length
				? paths.map((path, index) => (
						<Fragment key={path}>
							{index ? ', ' : null}
							<code>{path}</code>
						</Fragment>
					))
				: 'parts of it'}
			. Use Raw Configuration to keep them.
		</StatusBanner>
	);
}
