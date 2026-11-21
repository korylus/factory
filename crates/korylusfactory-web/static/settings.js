// 設定の画面の稼働のフォームで、保存中の再操作を止め、通信が途切れたことを知らせる。
// htmxが初期化するときの htmx:after:init を受けるため、htmxより前に読み込む。
(() => {
  const initializeOperationForm = (event) => {
    const form = event.target;
    if (form.id !== "operation-form") return;

    const operation = form.querySelector("#operation");
    const fallback = form.querySelector("[data-operation-fallback]");
    const error = form.querySelector("[data-operation-error]");
    fallback.hidden = true;

    // 応答でフォームが差し替わるまで、フォーカスを保ったままスイッチの再操作を止める。
    operation.addEventListener("click", (click) => {
      if (operation.hasAttribute("aria-disabled")) click.preventDefault();
    });
    form.addEventListener("htmx:before:request", () => {
      error.hidden = true;
      operation.setAttribute("aria-disabled", "true");
    });
    form.addEventListener("htmx:finally:request", () => {
      operation.removeAttribute("aria-disabled");
    });
    form.addEventListener("htmx:error", () => {
      // 応答が途切れたときは、開き直すまで保存の結果が分からない。
      operation.checked = operation.defaultChecked;
      error.hidden = false;
      error.focus();
    });
  };

  document.addEventListener("htmx:after:init", initializeOperationForm);
})();
