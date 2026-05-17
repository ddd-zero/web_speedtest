#!/usr/bin/env node

import { readFile, writeFile } from "node:fs/promises";
import { parseArgs } from "node:util";
import { fileURLToPath } from "node:url";

import { minify as minifyHtml } from "html-minifier-terser";
import JavaScriptObfuscator from "javascript-obfuscator";
import { parse } from "node-html-parser";
import { minify as minifyJs } from "terser";

const TERSER_OPTIONS = {
  compress: {
    passes: 2,
    drop_console: false
  },
  mangle: false,
  format: {
    comments: false
  }
};

const OBFUSCATOR_OPTIONS = {
  optionsPreset: "high-obfuscation",
  target: "browser",
  sourceMap: false,
  renameGlobals: false
};

const HTML_MINIFIER_OPTIONS = {
  collapseWhitespace: true,
  conservativeCollapse: true,
  removeComments: true,
  removeRedundantAttributes: true,
  removeScriptTypeAttributes: true,
  removeStyleLinkTypeAttributes: true,
  minifyCSS: true,
  // JS 已经经过 Terser 和 high-obfuscation，二次改写会增加破坏自防护代码的风险。
  minifyJS: false
};

const PARSE_OPTIONS = {
  comment: true,
  blockTextElements: {
    script: true,
    style: true,
    pre: true
  }
};

export async function processIndexHtml(html, { inputPath = "index.html" } = {}) {
  const root = parse(html, PARSE_OPTIONS);
  const inlineScripts = root
    .querySelectorAll("script")
    .filter(script => !script.getAttribute("src") && script.innerHTML.trim().length > 0);

  if (inlineScripts.length !== 1) {
    throw new Error(`${inputPath} 必须包含且仅包含一个内联 <script>，当前数量为 ${inlineScripts.length}`);
  }

  const script = inlineScripts[0];
  const minifiedScript = await minifyScript(script.innerHTML, inputPath);
  script.set_content(obfuscateScript(minifiedScript));

  return minifyHtml(root.toString(), HTML_MINIFIER_OPTIONS);
}

async function minifyScript(source, inputPath) {
  const result = await minifyJs(source, TERSER_OPTIONS);
  if (!result.code) {
    throw new Error(`${inputPath} 的内联脚本压缩后为空`);
  }
  return result.code;
}

function obfuscateScript(source) {
  return JavaScriptObfuscator.obfuscate(source, OBFUSCATOR_OPTIONS).getObfuscatedCode();
}

function parseCliArgs() {
  const { values } = parseArgs({
    options: {
      input: {
        type: "string",
        short: "i"
      },
      output: {
        type: "string",
        short: "o"
      }
    }
  });

  if (!values.input || !values.output) {
    throw new Error("用法: node scripts/process-index.mjs --input frontend/index.html --output frontend/index.html");
  }

  return {
    input: values.input,
    output: values.output
  };
}

async function main() {
  const { input, output } = parseCliArgs();
  const source = await readFile(input, "utf8");
  const processed = await processIndexHtml(source, { inputPath: input });
  await writeFile(output, processed, "utf8");
  console.log(`已处理 ${input} 并写入 ${output}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch(error => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
