/* -*- Mode: indent-tabs-mode: nil; js-indent-level: 2 -*- */
/* vim: set sts=2 sw=2 et tw=80: */
"use strict";

add_task(async function test_openPopup_requires_user_interaction() {
  async function backgroundScript() {
    browser.tabs.onUpdated.addListener(async (tabId, changeInfo) => {
      if (changeInfo.status != "complete") {
        return;
      }
      await browser.pageAction.show(tabId);

      await browser.test.assertRejects(
        browser.pageAction.openPopup(),
        "pageAction.openPopup may only be called from a user input handler",
        "The error is informative."
      );
      await browser.test.assertRejects(
        browser.sidebarAction.open(),
        "sidebarAction.open may only be called from a user input handler",
        "The error is informative."
      );
      await browser.test.assertRejects(
        browser.sidebarAction.close(),
        "sidebarAction.close may only be called from a user input handler",
        "The error is informative."
      );
      await browser.test.assertRejects(
        browser.sidebarAction.toggle(),
        "sidebarAction.toggle may only be called from a user input handler",
        "The error is informative."
      );

      browser.runtime.onMessage.addListener(async msg => {
        browser.test.assertEq(msg, "from-panel", "correct message received");
        browser.test.sendMessage("panel-opened");
      });

      browser.test.sendMessage("ready");
    });
    browser.tabs.create({ url: "tab.html" });
  }

  let extensionData = {
    background: backgroundScript,
    manifest: {
      browser_action: {
        default_popup: "panel.html",
      },
      page_action: {
        default_popup: "panel.html",
      },
      sidebar_action: {
        default_panel: "panel.html",
      },
    },
    // We don't want the panel open automatically, so need a non-default reason.
    startupReason: "APP_STARTUP",

    files: {
      "tab.html": `
      <!DOCTYPE html>
      <html><head><meta charset="utf-8"></head><body>
      <button id="openPageAction">openPageAction</button>
      <button id="openSidebarAction">openSidebarAction</button>
      <button id="closeSidebarAction">closeSidebarAction</button>
      <button id="toggleSidebarAction">toggleSidebarAction</button>
      <script src="tab.js"></script>
      </body></html>
      `,
      "panel.html": `
      <!DOCTYPE html>
      <html><head><meta charset="utf-8"></head><body>
      <script src="panel.js"></script>
      </body></html>
      `,
      "tab.js": function () {
        document.getElementById("openPageAction").addEventListener(
          "click",
          () => {
            browser.pageAction.openPopup();
          },
          { once: true }
        );
        document.getElementById("openSidebarAction").addEventListener(
          "click",
          () => {
            browser.sidebarAction.open();
          },
          { once: true }
        );
        document.getElementById("closeSidebarAction").addEventListener(
          "click",
          () => {
            browser.sidebarAction.close();
          },
          { once: true }
        );
        /* eslint-disable mozilla/balanced-listeners */
        document
          .getElementById("toggleSidebarAction")
          .addEventListener("click", () => {
            browser.sidebarAction.toggle();
          });
        /* eslint-enable mozilla/balanced-listeners */
      },
      "panel.js": function () {
        browser.runtime.sendMessage("from-panel");
        browser.test.onMessage.addListener(async msg => {
          browser.test.assertEq("window_close", msg, "Expected msg");
          window.close();
        });
      },
    },
  };

  let extension = ExtensionTestUtils.loadExtension(extensionData);

  async function click(id) {
    let open = extension.awaitMessage("panel-opened");
    await BrowserTestUtils.synthesizeMouseAtCenter(
      id,
      {},
      gBrowser.selectedBrowser
    );
    return open;
  }

  await extension.startup();
  await extension.awaitMessage("ready");

  await click("#openPageAction");
  closePageAction(extension);
  await new Promise(resolve => setTimeout(resolve, 0));

  await click("#openSidebarAction");

  await BrowserTestUtils.synthesizeMouseAtCenter(
    "#closeSidebarAction",
    {},
    gBrowser.selectedBrowser
  );
  await TestUtils.waitForCondition(() => !SidebarController.isOpen);

  await click("#toggleSidebarAction");
  await TestUtils.waitForCondition(() => SidebarController.isOpen);
  await BrowserTestUtils.synthesizeMouseAtCenter(
    "#toggleSidebarAction",
    {},
    gBrowser.selectedBrowser
  );
  await TestUtils.waitForCondition(() => !SidebarController.isOpen);

  await click("#toggleSidebarAction");
  await TestUtils.waitForCondition(() => SidebarController.isOpen);

  extension.sendMessage("window_close");
  await TestUtils.waitForCondition(() => !SidebarController.isOpen);

  BrowserTestUtils.removeTab(gBrowser.selectedTab);
  await extension.unload();
});
