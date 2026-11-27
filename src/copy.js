// Each copyable block gets a working button; without this script the block selects on one click.
for (const block of document.querySelectorAll(".copy")) {
  const button = block.querySelector("button");
  if (!navigator.clipboard) continue;
  button.hidden = false;
  button.addEventListener("click", async () => {
    await navigator.clipboard.writeText(block.querySelector("code").textContent);
    button.textContent = "Copied";
    setTimeout(() => (button.textContent = "Copy"), 1500);
  });
}
