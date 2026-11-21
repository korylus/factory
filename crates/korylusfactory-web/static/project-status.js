// プロジェクトの一覧と画面で、報告による停止・再開の変化を、フォーカスを動かさずに支援技術へ知らせる。
// data-project-status-view の領域と、差し替えの外に置いた project-status-announcement を持つ画面で読み込む。
(() => {
  const view = document.querySelector("[data-project-status-view]");
  const announcement = document.getElementById("project-status-announcement");

  const readProjects = () =>
    new Map(
      Array.from(view.querySelectorAll("[data-project-name]"), (element) => [
        element.dataset.projectName,
        {
          state: element.dataset.projectState,
          paused: element.hasAttribute("data-project-paused"),
          mismatch: element.querySelector("[data-operation-mismatch]")?.textContent.trim() ?? "",
        },
      ]),
    );

  // 最初に描いた状態を覚え、時刻の更新や一覧の並び替えでは通知しない。
  let previous = readProjects();
  view.addEventListener("htmx:after:settle", (event) => {
    if (event.target !== view) return;

    const current = readProjects();
    const messages = [];
    for (const [name, project] of current) {
      const before = previous.get(name);
      if (!before) continue;

      const changes = [];
      if (project.paused !== before.paused || (before.mismatch && !project.mismatch)) {
        changes.push(project.state);
      }
      if (project.mismatch && project.mismatch !== before.mismatch) {
        changes.push(project.mismatch);
      }
      if (changes.length) {
        messages.push([name, ...changes].join("。") + "。");
      }
    }

    previous = current;
    if (messages.length) announcement.textContent = messages.join(" ");
  });
})();
