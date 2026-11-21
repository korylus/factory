import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { generateTokensCSS } from "./generate-tokens.mjs";

// 各テストで一部を書き換えられるよう、呼び出すたびに新しいオブジェクトを返す。
function buildTokens() {
  return {
    color: {
      themes: [{ id: "light" }, { id: "dark" }],
      tokens: [
        { name: "ring", value: { light: "{primary}", dark: "{primary}" } },
        { name: "primary", value: { light: "oklch(0.205 0 0)", dark: "oklch(0.922 0 0)" } },
        { name: "warning", value: { light: "oklch(0.72 0.15 70)", dark: "oklch(0.8 0.15 75)" } },
      ],
    },
    radius: {
      tokens: [{ name: "radius", value: "0.625rem" }],
    },
    fontSize: {
      tokens: [
        { name: "text-xs", value: "0.8125rem" },
        { name: "text-xs--line-height", value: "calc(1.125 / 0.8125)" },
      ],
    },
    type: {
      families: { sans: "ui-sans-serif, sans-serif", mono: "ui-monospace, monospace" },
      fonts: [],
      groups: [],
    },
  };
}

describe("generateTokensCSS", () => {
  it("テーマごとの色・:rootの値・Basecoatに無い色の対応付けを順に出力する", () => {
    const css = generateTokensCSS(buildTokens());

    assert.equal(
      css,
      `/* assets/tokens.json から scripts/generate-tokens.mjs で生成する。直接編集せず、\`make web-assets\` で作り直すこと。 */

:root {
  --ring: var(--primary);
  --primary: oklch(0.205 0 0);
  --warning: oklch(0.72 0.15 70);
}

.dark {
  --ring: var(--primary);
  --primary: oklch(0.922 0 0);
  --warning: oklch(0.8 0.15 75);
}

:root {
  --radius: 0.625rem;
  --text-xs: 0.8125rem;
  --text-xs--line-height: calc(1.125 / 0.8125);
  --font-sans: ui-sans-serif, sans-serif;
  --font-mono: ui-monospace, monospace;
}

@theme inline {
  --color-warning: var(--warning);
}
`,
    );
  });

  it("Basecoatに無い色が無ければ @theme inline を出力しない", () => {
    const tokens = buildTokens();
    tokens.color.tokens = tokens.color.tokens.filter((token) => token.name !== "warning");

    assert.ok(!generateTokensCSS(tokens).includes("@theme"), "@theme inline を出力した");
  });

  it("fontSize が無くても出力できる", () => {
    const tokens = buildTokens();
    delete tokens.fontSize;

    assert.ok(!generateTokensCSS(tokens).includes("--text-xs"), "無い文字サイズを出力した");
  });

  it("テーマの値が欠けた色のトークンがあればエラーにする", () => {
    const tokens = buildTokens();
    delete tokens.color.tokens[2].value.dark;

    assert.throws(() => generateTokensCSS(tokens), {
      message: '色のトークン "warning" にテーマ "dark" の値がありません',
    });
  });

  it("存在しないトークンへの参照があればエラーにする", () => {
    const tokens = buildTokens();
    tokens.color.tokens[0].value.light = "{brand}";

    assert.throws(() => generateTokensCSS(tokens), {
      message: '色のトークン "ring" が存在しないトークン "brand" を参照しています',
    });
  });

  it("セレクターを定義していないテーマがあればエラーにする", () => {
    const tokens = buildTokens();
    tokens.color.themes.push({ id: "sepia" });

    assert.throws(() => generateTokensCSS(tokens), {
      message: 'テーマ "sepia" のセレクターが THEME_SELECTORS に定義されていません',
    });
  });

  it("文字スタイルが定義されていればエラーにする", () => {
    const tokens = buildTokens();
    tokens.type.groups.push({ name: "Text", family: "sans", styles: [] });

    assert.throws(() => generateTokensCSS(tokens), /文字スタイル \(type.groups\) は出力できません/);
  });
});
