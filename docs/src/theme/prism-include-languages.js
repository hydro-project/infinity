import siteConfig from "@generated/docusaurus.config";

// Rustdoc-style code block support: register the `rust,ignore` /
// `rust,no_run` / `compile_fail` info strings as Rust grammars, and strip
// rustdoc's hidden lines (lines starting with `# `) from rendered output.
// Adapted from the Hydro project's docs.
export default function prismIncludeLanguages(PrismObject) {
  const {
    themeConfig: { prism },
  } = siteConfig;
  const { additionalLanguages } = prism;

  const PrismBefore = globalThis.Prism;
  globalThis.Prism = PrismObject;
  additionalLanguages.forEach((lang) => {
    // eslint-disable-next-line global-require, import/no-dynamic-require
    require(`prismjs/components/prism-${lang}`);
  });
  PrismObject.languages["rust,ignore"] = PrismObject.languages.rust;
  PrismObject.languages["rust,no_run"] = PrismObject.languages.rust;
  PrismObject.languages["compile_fail"] = PrismObject.languages.rust;

  const origTokenize = PrismObject.tokenize;
  PrismObject.hooks.add("after-tokenize", function (env) {
    if (
      env.language === "rust" ||
      env.language === "rust,ignore" ||
      env.language === "rust,no_run" ||
      env.language === "compile_fail"
    ) {
      const code = env.code
        .split("\n")
        .filter((line) => !line.trimStart().startsWith("# ") && line.trim() !== "#")
        .join("\n");
      env.tokens = origTokenize(code, env.grammar);
    }
  });

  delete globalThis.Prism;
  if (typeof PrismBefore !== "undefined") {
    globalThis.Prism = PrismObject;
  }
}
