// Files bundled as text by the `rules` in wrangler.jsonc.
declare module "*.sh" {
	const text: string;
	export default text;
}
