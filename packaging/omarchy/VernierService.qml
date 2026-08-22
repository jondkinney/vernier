import QtQuick
import Quickshell
import Quickshell.Io

Item {
    id: root

    property bool checked: false
    property bool installed: false
    property bool running: false
    property bool apiSupported: false
    property bool measurementActive: false
    property bool overlayVisible: false
    property bool backgroundMode: false
    property bool companionAttached: false
    property bool activationPending: false
    property int companionActivationProtocol: 0
    property bool clearSupported: false
    property string toggleShortcut: ""
    property string clearAndExitShortcut: ""
    readonly property bool clearPending: clearProcess.running
    readonly property bool hasClearableContent: heldRectCount + guideCount + stuckMeasurementCount > 0
    readonly property string toggleShortcutHint: shortcutDisplay(toggleShortcut)
    readonly property string clearAndExitShortcutHint: shortcutDisplay(clearAndExitShortcut)
    property bool prepareActivationPending: false
    property bool prepareActivationWanted: false
    property string prepareActivationOwner: ""
    property string preparedActivationToken: ""
    property double preparedActivationExpiresAt: 0
    property string activationCommandToken: ""
    property string activationCommandOwner: ""
    property string activationError: ""
    property string clearError: ""
    property int heldRectCount: 0
    property int guideCount: 0
    property int stuckMeasurementCount: 0
    property string interactionMode: "idle"
    property string appVersion: ""
    property string buildId: ""
    property string binaryPath: "vernier"
    property string installState: "none"
    property bool installLaunchPending: false
    property string lastError: ""
    property string actionStatus: ""
    readonly property bool busy: statusProcess.running || prepareActivationProcess.running || preparedActivationProcess.running || clearProcess.running
    readonly property bool supportsPreparedActivation: apiSupported && companionActivationProtocol >= 1
    readonly property bool preparedActivationReady: preparedActivationToken !== "" && Date.now() < preparedActivationExpiresAt
    readonly property string installerPath: localPath(Qt.resolvedUrl("scripts/install-vernier"))
    readonly property string localReleaseBinary: localPath(Qt.resolvedUrl("../../target/release/vernier"))

    signal activationPrepared(string ownerId)
    signal activationPreparationFailed(string ownerId, string message)
    signal activationPreparationRejected(string ownerId, string message)
    signal preparedActivationExpired(string ownerId, string message)
    signal preparedActivationFailed(string ownerId, string message)

    function localPath(url) {
        var value = String(url || "");
        if (value.indexOf("file://") === 0)
            value = decodeURIComponent(value.substring(7));

        return value;
    }

    function cleanInstallState(value) {
        var state = String(value || "").trim();
        return state === "installing" || state === "ready" || state === "failed" ? state : "none";
    }

    function shellQuote(value) {
        return "'" + String(value || "").replace(/'/g, "'\\''") + "'";
    }

    function clearDaemonState() {
        apiSupported = false;
        measurementActive = false;
        overlayVisible = false;
        backgroundMode = false;
        companionAttached = false;
        companionActivationProtocol = 0;
        clearSupported = false;
        toggleShortcut = "";
        clearAndExitShortcut = "";
        activationPending = false;
        prepareActivationWanted = false;
        prepareActivationOwner = "";
        clearPreparedActivationLocal();
        clearError = "";
        heldRectCount = 0;
        guideCount = 0;
        stuckMeasurementCount = 0;
        interactionMode = "idle";
        appVersion = "";
        buildId = "";
    }

    function clearPreparedActivationLocal() {
        preparedActivationExpiryTimer.stop();
        preparedActivationToken = "";
        preparedActivationExpiresAt = 0;
    }

    function processError(raw, fallback) {
        var message = String(raw || "").trim();
        if (message === "")
            return fallback;

        var lines = message.split("\n");
        return lines[lines.length - 1].trim() || fallback;
    }

    function shortcutDisplay(canonical) {
        var value = String(canonical || "").trim();
        if (value === "")
            return "";

        var glyphs = {
            "CTRL": "⌃",
            "SHIFT": "⇧",
            "ALT": "⌥",
            "SUPER": "\ue900",
            "ENTER": "↵",
            "ESC": "⎋",
            "TAB": "⇥",
            "BACKSPACE": "⌫",
            "DELETE": "⌦",
            "UP": "↑",
            "DOWN": "↓",
            "LEFT": "←",
            "RIGHT": "→",
            "SPACE": "␣",
            "PLUS": "+",
            "MINUS": "-",
            "EQUAL": "=",
            "UNDERSCORE": "_"
        };
        return value.split("+").map(function(token) {
            return glyphs[token] || token;
        }).join(" ");
    }

    function vernierCommand(args) {
        var command = [binaryPath];
        for (var i = 0; i < args.length; i++) command.push(args[i])
        return command;
    }

    function applyStatus(raw) {
        var lines = String(raw || "").trim().split("\n");
        if (lines.length === 0)
            return ;

        if (lines[0].indexOf("binary:") === 0) {
            var resolvedBinary = lines.shift().substring(7);
            if (resolvedBinary !== "")
                binaryPath = resolvedBinary;

        }
        if (lines.length > 0 && lines[0].indexOf("install:") === 0) {
            var observedInstallState = cleanInstallState(lines.shift().substring(8));
            if (observedInstallState !== "none") {
                var installationChanged = installLaunchPending || installState !== observedInstallState;
                installState = observedInstallState;
                installLaunchPending = false;
                installLaunchTimer.stop();
                if (installationChanged)
                    actionStatus = "";

            } else if (!installLaunchPending && installState !== "failed") {
                installState = "none";
            }
        }
        var payload = lines.join("\n").trim();
        checked = true;
        lastError = "";
        if (payload === "missing" || payload === "") {
            installed = false;
            running = false;
            clearDaemonState();
            return ;
        }
        installed = true;
        if (payload === "stopped") {
            running = false;
            clearDaemonState();
            return ;
        }
        if (payload === "legacy") {
            running = true;
            clearDaemonState();
            return ;
        }
        var status;
        try {
            status = JSON.parse(payload);
        } catch (error) {
            running = true;
            clearDaemonState();
            lastError = "Vernier returned an unreadable status";
            return ;
        }
        running = true;
        apiSupported = Number(status.schema_version) === 1;
        if (!apiSupported) {
            clearDaemonState();
            lastError = "Update the companion for this Vernier status version";
            return ;
        }
        measurementActive = status.measurement_active === true;
        if (measurementActive && activationPending) {
            activationPending = false;
            activationGuardTimer.stop();
        }
        overlayVisible = status.overlay_visible === true;
        backgroundMode = status.background_mode === true;
        companionAttached = status.companion_attached === true;
        companionActivationProtocol = Math.max(0, Number(status.companion_activation_protocol) || 0);
        clearSupported = status.clear_supported === true;
        toggleShortcut = String(status.toggle_shortcut || "");
        clearAndExitShortcut = String(status.clear_and_exit_shortcut || "");
        heldRectCount = Math.max(0, Number(status.held_rect_count) || 0);
        guideCount = Math.max(0, Number(status.guide_count) || 0);
        stuckMeasurementCount = Math.max(0, Number(status.stuck_measurement_count) || 0);
        if (!hasClearableContent)
            clearError = "";

        interactionMode = String(status.interaction_mode || "idle");
        appVersion = String(status.app_version || "");
        buildId = String(status.build_id || "");
        if (measurementActive)
            activationError = "";

    }

    function refresh() {
        if (enabled && !statusProcess.running)
            statusProcess.running = true;

    }

    function refreshSoon(message) {
        actionStatus = message || "";
        settleTimer.ticks = 0;
        settleTimer.restart();
    }

    function toggleMeasurement() {
        if (!running)
            return ;

        var activate = !measurementActive;
        cancelPreparedActivation();
        if (apiSupported) {
            measurementActive = activate;
            if (activate) {
                activationPending = true;
                activationGuardTimer.restart();
            } else {
                activationPending = false;
                activationGuardTimer.stop();
            }
            Quickshell.execDetached(vernierCommand([activate ? "activate" : "deactivate"]));
        } else {
            Quickshell.execDetached(vernierCommand(["toggle"]));
        }
        refreshSoon(activate ? "Activating…" : "Returning to background…");
    }

    function clearMeasurements() {
        if (!running || !apiSupported || !clearSupported || clearPending)
            return ;

        clearError = "";
        actionStatus = "Clearing…";
        clearProcess.running = true;
    }

    // Capture a clean frame before the popup is ever mapped. The daemon keeps
    // it behind an opaque, expiring token; opening the popup only happens after
    // this synchronous command has returned successfully in the worker Process.
    function prepareActivation(ownerId) {
        if (!running || measurementActive || !supportsPreparedActivation)
            return false;

        var requestOwner = String(ownerId || "").trim();
        if (requestOwner === "")
            return false;

        if (prepareActivationOwner !== "" && prepareActivationOwner !== requestOwner && (prepareActivationPending || prepareActivationWanted || preparedActivationToken !== "")) {
            activationPreparationRejected(requestOwner, "Vernier is already being opened on another display.");
            return false;
        }
        activationError = "";
        prepareActivationOwner = requestOwner;
        prepareActivationWanted = true;
        actionStatus = "Preparing a clean snapshot…";
        if (prepareActivationPending)
            return true;

        if (preparedActivationReady) {
            Qt.callLater(function() {
                if (root.prepareActivationWanted)
                    root.activationPrepared(root.prepareActivationOwner);

            });
            return true;
        }
        clearPreparedActivationLocal();
        prepareActivationPending = true;
        // Let the requesting BarWidget claim the popout coordinator and begin
        // closing any currently mapped popout before the command is spawned.
        Qt.callLater(function() {
            if (!root.prepareActivationPending)
                return ;

            if (!root.prepareActivationWanted) {
                root.prepareActivationPending = false;
                return ;
            }
            if (!prepareActivationProcess.running)
                prepareActivationProcess.running = true;

        });
        return true;
    }

    // Cancel means "do not open" even if prepare is already in flight. If the
    // worker wins the race and returns a token, its exit handler releases it.
    function cancelPreparedActivationToken(token) {
        var opaqueToken = String(token || "").trim();
        if (opaqueToken === "")
            return false;

        Quickshell.execDetached(vernierCommand(["companion", "cancel-activate", opaqueToken]));
        return true;
    }

    function cancelPreparedActivation(ownerId) {
        var requestOwner = String(ownerId || "").trim();
        if (requestOwner !== "" && prepareActivationOwner !== "" && prepareActivationOwner !== requestOwner)
            return false;

        prepareActivationWanted = false;
        prepareActivationOwner = "";
        actionStatus = "";
        var token = preparedActivationToken;
        clearPreparedActivationLocal();
        cancelPreparedActivationToken(token);
        return true;
    }

    // Transfer ownership to the BarWidget while PopupCard completes its stock
    // fade. `close()` can then run without cancelling the frame being consumed.
    function takePreparedActivationToken(ownerId) {
        var requestOwner = String(ownerId || "").trim();
        if (requestOwner === "" || prepareActivationOwner !== requestOwner) {
            activationPreparationRejected(requestOwner, "This prepared snapshot belongs to another display.");
            return "";
        }
        if (!preparedActivationReady) {
            clearPreparedActivationLocal();
            prepareActivationOwner = "";
            activationError = "The prepared snapshot expired. Open Vernier again to retry.";
            preparedActivationExpired(requestOwner, activationError);
            return "";
        }
        var token = preparedActivationToken;
        clearPreparedActivationLocal();
        prepareActivationWanted = false;
        prepareActivationOwner = "";
        return token;
    }

    function activatePreparedToken(token, ownerId) {
        var opaqueToken = String(token || "").trim();
        var requestOwner = String(ownerId || "").trim();
        if (opaqueToken === "")
            return false;

        if (preparedActivationProcess.running) {
            Quickshell.execDetached(vernierCommand(["companion", "cancel-activate", opaqueToken]));
            activationError = "Vernier is already activating another prepared snapshot.";
            preparedActivationFailed(requestOwner, activationError);
            return false;
        }
        activationError = "";
        actionStatus = "Activating…";
        activationPending = true;
        activationCommandToken = opaqueToken;
        activationCommandOwner = requestOwner;
        preparedActivationProcess.running = true;
        return true;
    }

    function openPreferences() {
        if (!installed)
            return ;

        Quickshell.execDetached(["uwsm-app", "--", binaryPath, "prefs"]);
    }

    function start() {
        if (!installed || running)
            return ;

        // A plugin checkout can briefly be newer than the AUR package while a
        // Vernier release propagates. Prefer the idempotent companion-aware
        // command, but keep older installations launchable with a bare start.
        Quickshell.execDetached(["bash", "-c", "bin=$1; if \"$bin\" start --help >/dev/null 2>&1; then exec uwsm-app -s b -t service -a vernier -- \"$bin\" start; else exec uwsm-app -s b -t service -a vernier -- \"$bin\"; fi", "vernier-companion-start", binaryPath]);
        refreshSoon("Starting Vernier…");
    }

    function quit() {
        if (!running)
            return ;

        Quickshell.execDetached(vernierCommand(["quit"]));
        running = false;
        clearDaemonState();
        refreshSoon("Stopping Vernier…");
    }

    function install() {
        if (installed || installState === "installing" || installerPath === "")
            return ;

        installState = "installing";
        installLaunchPending = true;
        actionStatus = "Installer opened in a terminal";
        Quickshell.execDetached(["omarchy-launch-floating-terminal-with-presentation", "bash " + shellQuote(installerPath)]);
        installLaunchTimer.restart();
    }

    Timer {
        id: pollTimer

        interval: 1000
        repeat: true
        running: root.enabled
        triggeredOnStart: true
        onTriggered: root.refresh()
    }

    Timer {
        id: heartbeatTimer

        interval: 5000
        repeat: true
        running: root.enabled && root.running && root.apiSupported
        triggeredOnStart: true
        onTriggered: {
            if (!heartbeatProcess.running)
                heartbeatProcess.running = true;

        }
    }

    Timer {
        id: settleTimer

        property int ticks: 0

        interval: 500
        repeat: true
        running: false
        onTriggered: {
            ticks += 1;
            root.refresh();
            if (ticks >= 10) {
                ticks = 0;
                stop();
                root.actionStatus = "";
            }
        }
    }

    Timer {
        id: activationGuardTimer

        // Guard only the ordinary closed-popup/legacy activation path. Prepared
        // activation is tracked by preparedActivationProcess instead.
        interval: 650
        repeat: false
        onTriggered: root.activationPending = false
    }

    Timer {
        id: preparedActivationExpiryTimer

        interval: 300000
        repeat: false
        onTriggered: {
            var token = root.preparedActivationToken;
            var owner = root.prepareActivationOwner;
            root.clearPreparedActivationLocal();
            root.prepareActivationOwner = "";
            root.prepareActivationWanted = false;
            root.cancelPreparedActivationToken(token);
            root.activationError = "The prepared snapshot expired. Open Vernier again to retry.";
            root.preparedActivationExpired(owner, root.activationError);
        }
    }

    Timer {
        id: installLaunchTimer

        interval: 10000
        repeat: false
        onTriggered: {
            if (!root.installLaunchPending)
                return ;

            root.installLaunchPending = false;
            root.installState = "failed";
            root.actionStatus = "";
            root.lastError = "";
        }
    }

    Process {
        id: statusProcess

        running: false
        command: ["bash", "-c", "repo_bin=$1; current_bin=$2; path_bin=$(command -v vernier 2>/dev/null || true); override_bin=${VERNIER_COMPANION_BINARY:-}; supports_status() { [[ -x $1 ]] && timeout 2s \"$1\" status --help >/dev/null 2>&1; }; running_bin=; if [[ -n ${XDG_RUNTIME_DIR:-} && -S $XDG_RUNTIME_DIR/vernier.sock ]] && command -v fuser >/dev/null 2>&1; then running_pid=$(fuser \"$XDG_RUNTIME_DIR/vernier.sock\" 2>/dev/null | awk '{print $1}'); if [[ $running_pid =~ ^[0-9]+$ ]]; then running_bin=$(readlink -f -- \"/proc/$running_pid/exe\" 2>/dev/null || true); fi; fi; bin=; if [[ -n $override_bin && -x $override_bin ]]; then bin=$(readlink -f -- \"$override_bin\"); elif supports_status \"$running_bin\"; then bin=$running_bin; elif supports_status \"$current_bin\"; then bin=$current_bin; elif supports_status \"$repo_bin\"; then bin=$repo_bin; elif [[ -n $path_bin && -x $path_bin ]]; then bin=$path_bin; elif [[ -x $repo_bin ]]; then bin=$repo_bin; fi; printf 'binary:%s\\n' \"$bin\"; state=none; if [[ -n ${XDG_RUNTIME_DIR:-} ]]; then state_dir=\"$XDG_RUNTIME_DIR/vernier-companion\"; candidate=\"$state_dir/install-state\"; lock=\"$state_dir/install.lock\"; if [[ -r $candidate ]]; then read -r state < \"$candidate\" || state=none; fi; if [[ $state == installing && -e $lock ]] && flock -n \"$lock\" true 2>/dev/null; then state=failed; fi; fi; case $state in installing|ready|failed) ;; *) state=none ;; esac; printf 'install:%s\\n' \"$state\"; if [[ -z $bin ]]; then printf 'missing\\n'; elif status=$(timeout 2s \"$bin\" status 2>/dev/null) && [[ ${status:0:1} == '{' ]]; then printf '%s\\n' \"$status\"; elif pgrep -x vernier >/dev/null 2>&1; then printf 'legacy\\n'; else printf 'stopped\\n'; fi", "vernier-companion-status", root.localReleaseBinary, root.binaryPath]
        onExited: function(exitCode) {
            if (exitCode === 0) {
                root.applyStatus(statusStdout.text);
            } else {
                root.checked = true;
                root.lastError = "Could not check Vernier status";
            }
        }

        stdout: StdioCollector {
            id: statusStdout

            waitForEnd: true
        }

    }

    Process {
        id: prepareActivationProcess

        running: false
        command: root.vernierCommand(["companion", "prepare-activate"])
        onExited: function(exitCode) {
            var wanted = root.prepareActivationWanted;
            var owner = root.prepareActivationOwner;
            root.prepareActivationPending = false;
            if (exitCode !== 0) {
                root.prepareActivationWanted = false;
                root.prepareActivationOwner = "";
                root.actionStatus = "";
                if (wanted) {
                    root.activationError = root.processError(prepareActivationStderr.text, "Could not prepare a clean measurement snapshot.");
                    root.activationPreparationFailed(owner, root.activationError);
                }
                return ;
            }
            var response;
            try {
                response = JSON.parse(String(prepareActivationStdout.text || "").trim());
            } catch (error) {
                response = null;
            }
            var token = response ? String(response.token || "").trim() : "";
            var expiresInMs = response ? Number(response.expires_in_ms) : 0;
            if (token === "" || Number(response ? response.schema_version : 0) !== 1 || !(expiresInMs > 0)) {
                root.cancelPreparedActivationToken(token);
                root.prepareActivationWanted = false;
                root.prepareActivationOwner = "";
                root.actionStatus = "";
                if (wanted) {
                    root.activationError = "Vernier returned an invalid prepared snapshot.";
                    root.activationPreparationFailed(owner, root.activationError);
                }
                return ;
            }
            if (!wanted) {
                root.cancelPreparedActivationToken(token);
                return ;
            }
            root.preparedActivationToken = token;
            root.preparedActivationExpiresAt = Date.now() + expiresInMs;
            preparedActivationExpiryTimer.interval = Math.max(1, Math.min(2.14748e+09, Math.floor(expiresInMs)));
            preparedActivationExpiryTimer.restart();
            root.activationError = "";
            root.actionStatus = "";
            root.activationPrepared(owner);
        }

        stdout: StdioCollector {
            id: prepareActivationStdout

            waitForEnd: true
        }

        stderr: StdioCollector {
            id: prepareActivationStderr

            waitForEnd: true
        }

    }

    Process {
        id: preparedActivationProcess

        running: false
        command: root.vernierCommand(["companion", "activate", root.activationCommandToken])
        onExited: function(exitCode) {
            var owner = root.activationCommandOwner;
            root.activationCommandToken = "";
            root.activationCommandOwner = "";
            root.activationPending = false;
            if (exitCode === 0) {
                root.measurementActive = true;
                root.activationError = "";
                root.refreshSoon("Activating…");
            } else {
                root.measurementActive = false;
                root.actionStatus = "";
                root.activationError = root.processError(preparedActivationStderr.text, "The prepared snapshot expired. Open Vernier again to retry.");
                root.preparedActivationFailed(owner, root.activationError);
                root.refresh();
            }
        }

        stderr: StdioCollector {
            id: preparedActivationStderr

            waitForEnd: true
        }

    }

    Process {
        id: clearProcess

        running: false
        command: root.vernierCommand(["clear"])
        onExited: function(exitCode) {
            if (exitCode === 0) {
                root.heldRectCount = 0;
                root.guideCount = 0;
                root.stuckMeasurementCount = 0;
                root.backgroundMode = false;
                root.overlayVisible = root.measurementActive;
                root.interactionMode = root.measurementActive ? "hover" : "idle";
                root.clearError = "";
                root.refreshSoon("Cleared");
            } else {
                root.actionStatus = "";
                root.clearError = root.processError(clearStderr.text, "Could not clear Vernier measurements.");
                root.refresh();
            }
        }

        stderr: StdioCollector {
            id: clearStderr

            waitForEnd: true
        }

    }

    Process {
        id: heartbeatProcess

        running: false
        command: ["timeout", "2s", root.binaryPath, "companion", "attach"]
    }

}
