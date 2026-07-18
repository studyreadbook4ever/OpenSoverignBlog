/* Standalone article enhancement. All inputs remain untrusted text. */
document.querySelectorAll(".osb-math").forEach((node) => {
  const source = node.querySelector("code")?.textContent ?? node.textContent ?? "";
  if (!window.katex) return;
  window.katex.render(source, node, {
    displayMode: node.classList.contains("osb-math-display"),
    throwOnError: false,
    strict: "warn",
    trust: false,
  });
});
