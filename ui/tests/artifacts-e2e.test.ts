import { chromium } from "playwright";

async function main() {
  const browser = await chromium.launch({
    executablePath: "/usr/bin/chromium",
    headless: true,
  });
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await context.newPage();

  console.log("Navigating to http://localhost:1420/?mock=1...");
  await page.goto("http://localhost:1420/?mock=1");
  await page.waitForLoadState("networkidle");

  // Click on "Artifacts Showcase" session in sidebar
  console.log("Looking for Artifacts Showcase in sidebar...");
  const showcaseLink = page.locator(".conv", { hasText: "Artifacts Showcase" });
  await showcaseLink.waitFor({ state: "visible", timeout: 5000 });
  await showcaseLink.click();

  // Wait for conversation to load
  await page.waitForTimeout(500);

  // Verify inline artifact cards
  const cards = page.locator(".artifact-card");
  const cardCount = await cards.count();
  console.log(`Found ${cardCount} inline artifact cards in transcript.`);
  if (cardCount < 2) {
    throw new Error(`Expected at least 2 artifact cards, got ${cardCount}`);
  }

  // Check card content
  const card1Text = await cards.nth(0).innerText();
  const card2Text = await cards.nth(1).innerText();
  console.log("Card 1 text:", card1Text.replace(/\n/g, " | "));
  console.log("Card 2 text:", card2Text.replace(/\n/g, " | "));

  // Verify collapsed reasoning blocks on assistant messages
  const reasoningBlocks = page.locator(".reasoning-block");
  const rbCount = await reasoningBlocks.count();
  console.log(`Found ${rbCount} reasoning block(s).`);
  if (rbCount < 1) {
    throw new Error("Expected at least one reasoning block");
  }
  const firstReasoning = reasoningBlocks.first();
  await firstReasoning.waitFor({ state: "visible" });
  // Collapsed by default: no .reasoning-body
  const bodyCount = await firstReasoning.locator(".reasoning-body").count();
  console.log("Reasoning body visible (collapsed state):", bodyCount === 0 ? "no (correct)" : "yes (wrong)");
  if (bodyCount !== 0) throw new Error("Reasoning should start collapsed");

  // Toggle open
  await firstReasoning.locator(".reasoning-head").click();
  await page.waitForTimeout(200);
  const bodyCountOpen = await firstReasoning.locator(".reasoning-body").count();
  console.log("Reasoning body visible (expanded state):", bodyCountOpen === 1 ? "yes (correct)" : "no (wrong)");
  if (bodyCountOpen !== 1) throw new Error("Reasoning should expand on click");

  // Toggle closed again
  await firstReasoning.locator(".reasoning-head").click();
  await page.waitForTimeout(200);
  const bodyCountClosed = await firstReasoning.locator(".reasoning-body").count();
  if (bodyCountClosed !== 0) throw new Error("Reasoning should collapse on second click");
  console.log("Reasoning block toggle works!");

  // Verify artifact side panel
  const panel = page.locator(".artifact-panel");
  await panel.waitFor({ state: "visible", timeout: 5000 });
  console.log("Artifact side panel is visible!");

  // Verify version switcher
  const versionCounter = page.locator(".version-counter");
  const versionText = await versionCounter.innerText();
  console.log("Version counter:", versionText);

  // Verify Preview iframe
  const iframe = page.locator("iframe.artifact-preview-iframe");
  await iframe.waitFor({ state: "visible" });
  console.log("Preview iframe is rendered!");

  // Take screenshot of Preview mode
  await page.screenshot({ path: "/tmp/antigravity/artifact_preview.png" });
  console.log("Screenshot saved to /tmp/antigravity/artifact_preview.png");

  // Test Code tab
  console.log("Switching to Code tab...");
  const codeTab = page.locator(".artifact-tabs .tab-btn", { hasText: "Code" });
  await codeTab.click();
  await page.waitForTimeout(300);

  const codeBlock = page.locator(".artifact-code-pre code.hljs");
  await codeBlock.waitFor({ state: "visible" });
  console.log("Syntax-highlighted code block is visible!");
  await page.screenshot({ path: "/tmp/antigravity/artifact_code.png" });

  // Test version navigation
  console.log("Testing version switcher ◀...");
  const prevBtn = page.locator(".btn-ver-nav", { hasText: "◀" });
  await prevBtn.click();
  await page.waitForTimeout(300);
  const versionTextV1 = await versionCounter.innerText();
  console.log("Version after ◀ click:", versionTextV1);
  if (!versionTextV1.includes("v1 of 2")) {
    throw new Error(`Expected v1 of 2, got ${versionTextV1}`);
  }

  // Test switching back to Preview in v1
  const previewTab = page.locator(".artifact-tabs .tab-btn", { hasText: "Preview" });
  await previewTab.click();
  await page.waitForTimeout(300);

  // Test Close panel button
  console.log("Testing Close panel button...");
  const closeBtn = page.locator(".artifact-actions .btn-close");
  await closeBtn.click();
  await page.waitForTimeout(300);

  const panelCount = await page.locator(".artifact-panel").count();
  console.log("Artifact panel count after close:", panelCount);
  if (panelCount !== 0) {
    throw new Error("Panel should be closed");
  }

  // Test reopening by clicking an artifact card
  console.log("Reopening panel by clicking first card...");
  await cards.nth(0).click();
  await page.waitForTimeout(300);
  await panel.waitFor({ state: "visible" });
  console.log("Panel reopened successfully!");

  // Test live artifact generation via composer
  console.log("Testing live artifact generation in chat...");
  const textarea = page.locator(".composer-input");
  await textarea.fill("Please create a calc app artifact");
  const sendBtn = page.locator(".composer-send");
  await sendBtn.click();

  // Wait for streaming card and completion
  console.log("Waiting for streaming artifact card...");
  await page.waitForTimeout(1000);
  await page.screenshot({ path: "/tmp/antigravity/artifact_live_streaming.png" });

  // Wait until run finishes (stop_reason: finished)
  await page.waitForFunction(() => {
    return document.querySelectorAll(".artifact-card").length >= 3;
  }, { timeout: 10000 });
  await page.waitForTimeout(600);

  console.log("Live artifact was successfully generated and rendered!");
  await page.screenshot({ path: "/tmp/antigravity/artifact_live_completed.png" });

  await browser.close();
  console.log("All Playwright verification tests passed successfully!");
}

main().catch((err) => {
  console.error("Test failed:", err);
  process.exit(1);
});
