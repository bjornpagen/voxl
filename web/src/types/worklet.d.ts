// AudioWorkletGlobalScope, which TypeScript's libraries don't include.

/** A processor whose node the page made with `processorOptions` of type `O`. */
declare abstract class AudioWorkletProcessor<O = undefined> {
	readonly port: MessagePort;
	constructor(options: { readonly processorOptions: O });
	abstract process(
		inputs: Float32Array[][],
		outputs: Float32Array[][],
		parameters: Record<string, Float32Array>
	): boolean;
}
declare function registerProcessor<O>(
	name: string,
	ctor: new (options: {
		readonly processorOptions: O;
	}) => AudioWorkletProcessor<O>
): void;
