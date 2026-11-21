/** Context Broker Sync: the Obsidian plugin shell around the sync engine. */

import {
  type App,
  Notice,
  type ObsidianProtocolData,
  Platform,
  Plugin,
  PluginSettingTab,
  Setting,
  type TAbstractFile,
} from "obsidian";
import { type LastSync, MAX_PARALLEL_FILES, parsePluginData, type PluginData } from "./data";
import { isSyncablePath, MassDeletionError, type SyncSummary, syncVault } from "./engine";
import { errorMessage } from "./guards";
import {
  authorizationUrl,
  createPkce,
  discoverEndpoints,
  exchangeCode,
  OAuthError,
  refreshTokens,
  registerClient,
} from "./oauth";
import { obsidianHttp, VaultFileStore } from "./obsidian-io";
import { AuthRequiredError, HttpRemote, serverOrigin, type TokenSource } from "./remote";

const PROTOCOL_ACTION = "context-broker-sync";
const CHANGE_DEBOUNCE_MS = 3000;
const NOTICE_PREFIX = "Context Broker Sync: ";
const LOG_LIMIT_BYTES = 200_000;
const LOG_PROGRESS_MS = 5_000;
const MAX_LISTED_IN_NOTICE = 3;

type SyncStatus =
  | { kind: "idle" }
  | { kind: "syncing" }
  | { kind: "synced"; at: number }
  | { kind: "error"; message: string };

// Obsidian loads the plugin class from the module's default export.
export default class ContextBrokerSyncPlugin extends Plugin {
  private data: PluginData = parsePluginData(null, "");
  private status: SyncStatus = { kind: "idle" };
  private statusBar: HTMLElement | null = null;
  private settingTab: ContextBrokerSettingTab | null = null;
  private syncing = false;
  private syncAgain = false;
  private changeTimer: number | null = null;
  private intervalTimer: number | null = null;

  /**
   * Appends a line to `sync.log` in the plugin folder. Phones have no developer console, so this
   * file is how a stalled or failing sync is diagnosed. It is started afresh once it grows large.
   */
  private async log(message: string): Promise<void> {
    const folder = this.manifest.dir;
    if (folder === undefined) return;
    const path = `${folder}/sync.log`;
    try {
      const adapter = this.app.vault.adapter;
      const existing = await adapter.stat(path);
      const line = `${new Date().toISOString()} ${message}\n`;
      if (existing !== null && existing.size > LOG_LIMIT_BYTES) await adapter.write(path, line);
      else await adapter.append(path, line);
    } catch {
      // Logging must never break a sync.
    }
  }

  override async onload(): Promise<void> {
    const defaultDeviceName = Platform.isMobile ? "Mobile" : "Desktop";
    this.data = parsePluginData(await this.loadData(), defaultDeviceName);

    this.statusBar = this.addStatusBarItem();
    if (this.data.lastSync?.ok === true) this.status = { kind: "synced", at: this.data.lastSync.at };
    this.renderStatus();

    this.addRibbonIcon("refresh-cw", "Context Broker Sync: sync now", () => this.syncNow());
    this.addCommand({ id: "sync-now", name: "Sync now", callback: () => this.syncNow() });
    this.addCommand({
      id: "sync-now-allow-mass-deletion",
      name: "Sync now and accept files deleted on the server",
      callback: () => this.syncNow(true),
    });
    this.settingTab = new ContextBrokerSettingTab(this.app, this);
    this.addSettingTab(this.settingTab);
    this.registerObsidianProtocolHandler(PROTOCOL_ACTION, (params) => {
      // `finishSignIn` reports its own failures with a Notice and never rejects.
      void this.finishSignIn(params);
    });
    this.register(() => {
      if (this.changeTimer !== null) window.clearTimeout(this.changeTimer);
    });

    // Before the layout is ready the vault fires "create" for every existing file.
    this.app.workspace.onLayoutReady(() => {
      const onChange = (file: TAbstractFile): void => this.onVaultChange(file);
      this.registerEvent(this.app.vault.on("create", onChange));
      this.registerEvent(this.app.vault.on("modify", onChange));
      this.registerEvent(this.app.vault.on("delete", onChange));
      this.registerEvent(this.app.vault.on("rename", onChange));
      this.restartInterval();
      this.syncNow();
    });
  }

  get pluginData(): PluginData {
    return this.data;
  }

  get signedIn(): boolean {
    return this.currentTokens() !== null;
  }

  async saveSettings(): Promise<void> {
    await this.saveData(this.data);
    this.restartInterval();
  }

  /** Starts a sync, or queues one more run when a sync is already going. */
  syncNow(allowMassDeletion = false): void {
    // `runSyncs` reports every failure through the status bar and a Notice and never rejects.
    void this.runSyncs(allowMassDeletion);
  }

  /** Discovers the OAuth endpoints, registers this client if needed and opens the browser. */
  async startSignIn(): Promise<void> {
    try {
      const origin = serverOrigin(this.data.settings.serverUrl);
      const endpoints = await discoverEndpoints(obsidianHttp, origin);
      const known = this.data.auth?.origin === origin ? this.data.auth : null;
      const clientId = known?.clientId ?? (await registerClient(obsidianHttp, endpoints));
      const pkce = await createPkce();
      this.data.auth = { origin, clientId, endpoints, tokens: known?.tokens ?? null };
      this.data.pendingSignIn = {
        origin,
        clientId,
        endpoints,
        verifier: pkce.verifier,
        state: pkce.state,
      };
      await this.saveData(this.data);
      window.open(authorizationUrl(endpoints, clientId, pkce, origin));
    } catch (error) {
      new Notice(`${NOTICE_PREFIX}sign-in could not start. ${errorMessage(error)}`, 10_000);
    }
  }

  async signOut(): Promise<void> {
    // The client registration goes too, so signing in again starts from a clean slate.
    this.data.auth = null;
    this.data.pendingSignIn = null;
    await this.saveData(this.data);
    this.settingTab?.display();
  }

  /** Handles the `obsidian://context-broker-sync?code=...&state=...` redirect. */
  private async finishSignIn(params: ObsidianProtocolData): Promise<void> {
    try {
      const pending = this.data.pendingSignIn;
      if (pending === null) throw new OAuthError("No sign-in is in progress.");
      const failure = params["error"];
      if (failure !== undefined) throw new OAuthError(`The server refused sign-in (${failure}).`);
      const code = params["code"];
      // The state check proves this redirect answers the request this device made.
      if (code === undefined || params["state"] !== pending.state) {
        throw new OAuthError("The sign-in response does not match the request.");
      }
      const tokens = await exchangeCode(
        obsidianHttp,
        pending.endpoints,
        pending.clientId,
        code,
        pending.verifier,
        pending.origin,
      );
      this.data.auth = {
        origin: pending.origin,
        clientId: pending.clientId,
        endpoints: pending.endpoints,
        tokens,
      };
      this.data.pendingSignIn = null;
      await this.saveData(this.data);
      new Notice(`${NOTICE_PREFIX}signed in.`);
      this.settingTab?.display();
      this.syncNow();
    } catch (error) {
      new Notice(`${NOTICE_PREFIX}sign-in failed. ${errorMessage(error)}`, 10_000);
    }
  }

  private currentTokens(): { accessToken: string; refreshToken: string | null } | null {
    const auth = this.data.auth;
    if (auth === null || auth.tokens === null) return null;
    let origin: string;
    try {
      origin = serverOrigin(this.data.settings.serverUrl);
    } catch (error) {
      if (error instanceof TypeError) return null;
      throw error;
    }
    // Tokens issued for one server are never sent to another.
    return auth.origin === origin ? auth.tokens : null;
  }

  private tokenSource(): TokenSource {
    return {
      accessToken: () => this.currentTokens()?.accessToken ?? null,
      refresh: async () => {
        const auth = this.data.auth;
        const refreshToken = this.currentTokens()?.refreshToken ?? null;
        if (auth === null || refreshToken === null) return null;
        try {
          auth.tokens = await refreshTokens(
            obsidianHttp,
            auth.endpoints,
            auth.clientId,
            refreshToken,
            auth.origin,
          );
        } catch (error) {
          // The authorization server rejected the refresh token: the user has to sign in again.
          if (!(error instanceof OAuthError)) throw error;
          auth.tokens = null;
        }
        await this.saveData(this.data);
        return auth.tokens?.accessToken ?? null;
      },
    };
  }

  private onVaultChange(file: TAbstractFile): void {
    if (!this.data.settings.syncOnChange || !isSyncablePath(file.path)) return;
    if (this.changeTimer !== null) window.clearTimeout(this.changeTimer);
    this.changeTimer = window.setTimeout(() => {
      this.changeTimer = null;
      this.syncNow();
    }, CHANGE_DEBOUNCE_MS);
  }

  private restartInterval(): void {
    if (this.intervalTimer !== null) window.clearInterval(this.intervalTimer);
    this.intervalTimer = null;
    const minutes = this.data.settings.intervalMinutes;
    if (minutes <= 0) return;
    this.intervalTimer = window.setInterval(() => this.syncNow(), minutes * 60_000);
    this.registerInterval(this.intervalTimer);
  }

  /** One sync at a time. A request that arrives during a sync causes exactly one more run. */
  private async runSyncs(allowMassDeletion: boolean): Promise<void> {
    if (this.syncing) {
      this.syncAgain = true;
      return;
    }
    this.syncing = true;
    try {
      let allow = allowMassDeletion;
      let succeeded: boolean;
      do {
        this.syncAgain = false;
        succeeded = await this.syncOnce(allow);
        allow = false;
        // After a failure the next attempt waits for the timer or the next change, so an
        // unreachable server is not hammered.
      } while (succeeded && this.syncAgain);
    } finally {
      this.syncing = false;
    }
  }

  private async syncOnce(allowMassDeletion: boolean): Promise<boolean> {
    const serverUrl = this.data.settings.serverUrl.trim();
    if (serverUrl === "") {
      this.setStatus({ kind: "error", message: "Set the server URL in the plugin settings." });
      return false;
    }
    this.setStatus({ kind: "syncing" });
    try {
      const origin = serverOrigin(serverUrl);
      if (this.data.stateOrigin !== origin) {
        // State from another server would make every file look deleted on this one.
        this.data.syncState = {};
        this.data.stateOrigin = origin;
      }
      // Progress goes to the log every few seconds, with the files in flight, so a sync that
      // stops moving shows where.
      const inFlight = new Set<string>();
      let done = 0;
      let total = 0;
      await this.log(`sync started against ${origin}`);
      const watch = window.setInterval(() => {
        void this.log(`progress ${done}/${total}; working on: ${[...inFlight].join(" | ") || "nothing"}`);
      }, LOG_PROGRESS_MS);
      try {
        const summary = await syncVault({
          local: new VaultFileStore(this.app),
          remote: new HttpRemote(origin, obsidianHttp, this.tokenSource()),
          state: this.data.syncState,
          persist: async () => {
            await this.saveData(this.data);
          },
          deviceName: this.data.settings.deviceName,
          concurrency: this.data.settings.parallelFiles,
          allowMassDeletion,
          onPath: (path, phase, count) => {
            total = count;
            if (phase === "start") inFlight.add(path);
            else {
              inFlight.delete(path);
              done += 1;
            }
          },
        });
        await this.log(
          `sync finished: ${summary.uploaded} uploaded, ${summary.downloaded} downloaded, ` +
            `${summary.conflicts.length} conflicts, ${summary.errors.length} errors` +
            summary.errors.slice(0, 5).map((failure) => `; ${failure.path}: ${failure.message}`).join(""),
        );
        await this.report(summary);
        return summary.errors.length === 0;
      } finally {
        window.clearInterval(watch);
      }
    } catch (error) {
      await this.log(`sync failed: ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`);
      await this.reportFailure(error);
      return false;
    }
  }

  private async report(summary: SyncSummary): Promise<void> {
    const deleted = summary.deletedLocal + summary.deletedRemote;
    const text =
      `${summary.uploaded} uploaded, ${summary.downloaded} downloaded, ${deleted} deleted, ` +
      `${summary.conflicts.length} conflicts, ${summary.errors.length} errors`;
    const ok = summary.errors.length === 0;
    await this.finish({ at: Date.now(), ok, text });

    if (summary.conflicts.length > 0) {
      const lines = summary.conflicts.map(
        (conflict) => `"${conflict.path}": your version was kept as "${conflict.conflictPath}"`,
      );
      new Notice(`${NOTICE_PREFIX}conflicting edits.\n${listed(lines)}`, 0);
    }
    if (!ok) {
      const lines = summary.errors.map((failure) => `"${failure.path}": ${failure.message}`);
      const stopped = summary.completed ? "" : "\nThe sync stopped early.";
      new Notice(`${NOTICE_PREFIX}some files failed.\n${listed(lines)}${stopped}`, 10_000);
    }
  }

  private async reportFailure(error: unknown): Promise<void> {
    let text = errorMessage(error);
    if (error instanceof AuthRequiredError) {
      text = "Sign-in required. Open the plugin settings and choose Sign in.";
    } else if (error instanceof MassDeletionError) {
      text += ' Run the command "Sync now and accept files deleted on the server" to confirm.';
    }
    await this.finish({ at: Date.now(), ok: false, text });
    new Notice(NOTICE_PREFIX + text, 10_000);
  }

  private async finish(lastSync: LastSync): Promise<void> {
    this.data.lastSync = lastSync;
    this.setStatus(
      lastSync.ok ? { kind: "synced", at: lastSync.at } : { kind: "error", message: lastSync.text },
    );
    try {
      await this.saveData(this.data);
    } catch (error) {
      new Notice(`${NOTICE_PREFIX}could not save plugin data. ${errorMessage(error)}`, 10_000);
    }
  }

  private setStatus(status: SyncStatus): void {
    this.status = status;
    this.renderStatus();
  }

  private renderStatus(): void {
    if (this.statusBar === null) return;
    const status = this.status;
    let text: string;
    let detail: string;
    switch (status.kind) {
      case "idle":
        text = "idle";
        detail = "Not synced yet";
        break;
      case "syncing":
        text = "syncing…";
        detail = "Sync in progress";
        break;
      case "synced":
        text = `synced ${timeOfDay(status.at)}`;
        detail = this.data.lastSync?.text ?? "";
        break;
      case "error":
        text = "error";
        detail = status.message;
        break;
      default: {
        const unreachable: never = status;
        throw new Error(`Unknown sync status: ${String(unreachable)}`);
      }
    }
    this.statusBar.setText(`Broker sync: ${text}`);
    this.statusBar.setAttr("aria-label", detail);
  }
}

function timeOfDay(at: number): string {
  return new Date(at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function listed(lines: string[]): string {
  const shown = lines.slice(0, MAX_LISTED_IN_NOTICE).join("\n");
  const more = lines.length - MAX_LISTED_IN_NOTICE;
  return more > 0 ? `${shown}\n…and ${more} more` : shown;
}

class ContextBrokerSettingTab extends PluginSettingTab {
  private readonly plugin: ContextBrokerSyncPlugin;

  constructor(app: App, plugin: ContextBrokerSyncPlugin) {
    super(app, plugin);
    this.plugin = plugin;
  }

  override display(): void {
    const { containerEl, plugin } = this;
    const settings = plugin.pluginData.settings;
    // Saving reports its own failure; nothing awaits these input handlers.
    const save = (): void => {
      plugin.saveSettings().catch((error: unknown) => {
        new Notice(`${NOTICE_PREFIX}could not save settings. ${errorMessage(error)}`);
      });
    };
    containerEl.empty();

    new Setting(containerEl)
      .setName("Server URL")
      .setDesc("Address of the Context Broker server, for example https://broker.example.com")
      .addText((text) =>
        text
          .setPlaceholder("https://")
          .setValue(settings.serverUrl)
          .onChange((value) => {
            settings.serverUrl = value.trim();
            save();
          }),
      );

    new Setting(containerEl)
      .setName("Device name")
      .setDesc("Used in the name of conflict copies made on this device.")
      .addText((text) =>
        text.setValue(settings.deviceName).onChange((value) => {
          settings.deviceName = value.trim();
          save();
        }),
      );

    new Setting(containerEl)
      .setName("Sync interval (minutes)")
      .setDesc("How often to sync automatically. 0 turns the timer off.")
      .addText((text) =>
        text.setValue(String(settings.intervalMinutes)).onChange((value) => {
          const minutes = Number(value);
          if (!Number.isFinite(minutes) || minutes < 0) return;
          settings.intervalMinutes = minutes;
          save();
        }),
      );

    new Setting(containerEl)
      .setName("Files in parallel")
      .setDesc(
        `How many files are compared and transferred at the same time (1 to ${MAX_PARALLEL_FILES}). ` +
          "Higher is faster on a good connection, especially for the first sync of a large vault; lower it on a slow phone or network.",
      )
      .addText((text) =>
        text.setValue(String(settings.parallelFiles)).onChange((value) => {
          const count = Math.round(Number(value));
          if (!Number.isFinite(count) || count < 1 || count > MAX_PARALLEL_FILES) return;
          settings.parallelFiles = count;
          save();
        }),
      );

    new Setting(containerEl)
      .setName("Sync on change")
      .setDesc("Sync a few seconds after a note is created, edited, renamed or deleted.")
      .addToggle((toggle) =>
        toggle.setValue(settings.syncOnChange).onChange((value) => {
          settings.syncOnChange = value;
          save();
        }),
      );

    new Setting(containerEl)
      .setName("Account")
      .setDesc(
        plugin.signedIn
          ? "Signed in."
          : "Not signed in. Sign-in is only needed outside the server's trusted networks.",
      )
      .addButton((button) =>
        button.setButtonText("Sign in").onClick(() => {
          // `startSignIn` shows its own Notice on failure and never rejects.
          void plugin.startSignIn();
        }),
      )
      .addButton((button) =>
        button.setButtonText("Sign out").onClick(() => {
          plugin.signOut().catch((error: unknown) => {
            new Notice(`${NOTICE_PREFIX}could not sign out. ${errorMessage(error)}`);
          });
        }),
      );

    const lastSync = plugin.pluginData.lastSync;
    new Setting(containerEl)
      .setName("Last sync")
      .setDesc(
        lastSync === null
          ? "Never."
          : `${new Date(lastSync.at).toLocaleString()}: ${lastSync.text}`,
      )
      .addButton((button) => button.setButtonText("Sync now").onClick(() => plugin.syncNow()));
  }
}
