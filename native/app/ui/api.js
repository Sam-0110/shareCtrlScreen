// Every call into the Rust side (`#[tauri::command]`s in src/commands.rs) and
// every event it emits (src/bridge.rs, src/menu.rs, src/updater.rs) goes
// through this module, so the UI's whole contract with the backend is in one
// file. Argument names are the commands' parameter names (Tauri maps the
// camelCase keys onto the Rust snake_case parameters).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

/** Subscribe to a backend event; resolves to the unlisten function. */
export const on = (event, handler) => listen(event, handler);

// ---- config, names, recents -------------------------------------------------
export const getConfig = () => invoke('get_config');
export const saveSettings = (patch) => invoke('save_settings', { patch });
export const setRemoteName = (id, name) => invoke('set_remote_name', { id, name });
export const clearRecents = () => invoke('clear_recents');
export const getPlatformInfo = () => invoke('get_platform_info');
export const getServerStatus = () => invoke('get_server_status');

// ---- accounts (src/accounts.rs) ----------------------------------------------
// `account` (a key) signs in to that known account on its own server (the
// locked form); null uses the server in Settings.
export const accountSignIn = (username, password, keepSignedIn, account) =>
  invoke('account_sign_in', { username, password, keepSignedIn, account });
export const accountSignUp = (username, password, keepSignedIn) =>
  invoke('account_sign_up', { username, password, keepSignedIn });
export const accountResolveLocal = (choice) => invoke('account_resolve_local', { choice });
export const accountSwitch = (account) => invoke('account_switch', { account });
export const accountSignOut = () => invoke('account_sign_out');
export const accountRemove = (account) => invoke('account_remove', { account });
export const accountSync = () => invoke('account_sync');
export const accountWindowFocused = () => invoke('account_window_focused');

// ---- tabs (src/tabs.rs; plans/08a §4.1) -----------------------------------------
// The strip redraws only from `tabs-changed` snapshots; a command's answer is
// used only for `{ needsConfirm: n }` (more than 8 connected tabs, D2).
export const tabOpen = (remoteId, confirmed) => invoke('tab_open', { remoteId, confirmed });
export const tabConnect = (remoteId, confirmed) =>
  invoke('tab_connect', { remoteId, confirmed });
export const tabDisconnect = (remoteId) => invoke('tab_disconnect', { remoteId });
export const tabClose = (remoteId) => invoke('tab_close', { remoteId });
export const tabActivate = (remoteId) => invoke('tab_activate', { remoteId });
export const tabNext = () => invoke('tab_next');
export const tabPrev = () => invoke('tab_prev');
export const tabRename = (remoteId, name) => invoke('tab_rename', { remoteId, name });
export const tabMove = (remoteId, before) => invoke('tab_move', { remoteId, before });
export const tabConnectAll = (confirmed) => invoke('tab_connect_all', { confirmed });
export const tabPassword = (remoteId, password, remember) =>
  invoke('tab_password', { remoteId, password, remember });
export const tabForgetPassword = (remoteId) => invoke('tab_forget_password', { remoteId });
export const getSnapshot = () => invoke('get_snapshot');

// ---- answering, and the session on screen ---------------------------------------
export const approve = (from, decision) => invoke('approve', { from, decision });
export const endSession = () => invoke('end_session');

// ---- the live session -----------------------------------------------------------
export const setPermission = (value) => invoke('set_permission', { value });
export const setVideoVisible = (visible) => invoke('set_video_visible', { visible });
export const requestRefresh = () => invoke('request_refresh');
export const selectDisplay = (id) => invoke('select_display', { id });
export const popoutDisplay = (id) => invoke('popout_display', { id });

// ---- OS permissions -------------------------------------------------------------
export const getPermissions = () => invoke('get_permissions');
export const requestPermission = (name) => invoke('request_permission', { name });

// ---- OpenH264 and updates ---------------------------------------------------------
export const openh264Info = () => invoke('openh264_info');
export const openh264Download = () => invoke('openh264_download');
export const updaterCheck = () => invoke('updater_check');
export const updaterInstall = () => invoke('updater_install');
export const openReleasesPage = () => invoke('open_releases_page');
