// OSのライトとダークの設定を html の dark クラスに写す。
// Basecoatの dark: バリアントと color-scheme が html.dark を基準にするため。
// 描画の前に走るよう、<head> の最初のスクリプトとして同期で読み込む。
(() => {
  const query = matchMedia("(prefers-color-scheme: dark)");
  const apply = () => document.documentElement.classList.toggle("dark", query.matches);
  apply();
  query.addEventListener("change", apply);
})();
