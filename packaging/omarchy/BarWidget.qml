import QtQuick
import Quickshell
import qs.Commons
import qs.Ui

BarWidget {
    id: root

    property bool opened: false
    property bool openRequestPending: false
    property bool quickActivationRequestPending: false
    property bool preparationPopoutClaimed: false
    property string activationRequestOwner: ""
    property string activationRequestError: ""
    property string preparedTokenAfterClose: ""
    property bool legacyActivationAfterClose: false
    readonly property var sharedVernier: bar && bar.shell && typeof bar.shell.serviceFor === "function" ? bar.shell.serviceFor(moduleName) : null
    readonly property var vernier: sharedVernier || fallbackVernier
    readonly property bool activationAfterClosePending: preparedTokenAfterClose !== "" || legacyActivationAfterClose
    readonly property bool measurementTogglePending: activationAfterClosePending || vernier.activationPending === true
    readonly property color panelForeground: bar ? bar.foreground : Color.popups.text
    readonly property color barIconColor: bar ? bar.barForeground : Color.foreground
    readonly property string statusTitle: {
        if (openRequestPending || quickActivationRequestPending)
            return "Preparing Vernier…";

        if (!vernier.checked)
            return "Checking Vernier…";

        if (!vernier.installed)
            return "Vernier is not installed";

        if (!vernier.running)
            return "Vernier is stopped";

        if (!vernier.apiSupported)
            return "Vernier is running";

        if (vernier.measurementActive)
            return "Measuring";

        if (vernier.backgroundMode)
            return "Background mode";

        return "Ready";
    }
    readonly property string statusMeta: {
        if (openRequestPending || quickActivationRequestPending)
            return "CLEAN SNAPSHOT";

        if (!vernier.checked)
            return "CONNECTING";

        if (!vernier.installed)
            return vernier.installState === "failed" ? "INSTALL FAILED" : "FIRST LAUNCH";

        if (!vernier.running)
            return "OFFLINE";

        if (!vernier.apiSupported)
            return "LEGACY CONTROL";

        return vernier.measurementActive ? vernier.interactionMode.toUpperCase() : "IDLE";
    }

    function ownerId() {
        if (activationRequestOwner === "")
            activationRequestOwner = moduleName + ":" + Date.now() + ":" + Math.random();

        return activationRequestOwner;
    }

    function claimPopoutForPreparation() {
        if (!bar || typeof bar.requestPopout !== "function")
            return ;

        bar.requestPopout(root);
        preparationPopoutClaimed = true;
    }

    function releasePreparationPopout() {
        var ownsActivePopout = bar && "activePopout" in bar && bar.activePopout === root;
        if (!preparationPopoutClaimed && !ownsActivePopout)
            return ;

        if (bar && typeof bar.releasePopout === "function")
            bar.releasePopout(root);

        preparationPopoutClaimed = false;
    }

    function hasMappedPopout() {
        if (popup.visible)
            return true;

        return !!(bar && "activePopout" in bar && bar.activePopout && bar.activePopout !== root);
    }

    function beginQuickActivation() {
        if (measurementTogglePending || openRequestPending || quickActivationRequestPending)
            return ;

        if (!vernier.supportsPreparedActivation) {
            activationRequestError = "Close the open panel before measuring, or update Vernier for clean quick activation.";
            return ;
        }
        activationRequestError = "";
        quickActivationRequestPending = true;
        if (vernier.prepareActivation(ownerId()))
            claimPopoutForPreparation();
        else
            quickActivationRequestPending = false;
    }

    function open() {
        if (measurementTogglePending || quickActivationRequestPending)
            return ;

        // `WidgetButton.triggerPress()` normally clears this before a mouse
        // click reaches us. Shell summon/keyboard callers bypass that helper,
        // so clear it explicitly before the popup-free snapshot as well.
        if (typeof button.hideOwnTooltip === "function")
            button.hideOwnTooltip();

        if (vernier.running && vernier.supportsPreparedActivation && !vernier.measurementActive) {
            activationRequestError = "";
            openRequestPending = true;
            if (vernier.prepareActivation(ownerId()))
                claimPopoutForPreparation();
            else
                openRequestPending = false;
            return ;
        }
        opened = true;
        vernier.refresh();
    }

    function close() {
        openRequestPending = false;
        quickActivationRequestPending = false;
        opened = false;
        if (!activationAfterClosePending)
            vernier.cancelPreparedActivation(ownerId());

        releasePreparationPopout();
    }

    function toggle() {
        if (openRequestPending || quickActivationRequestPending) {
            close();
            return ;
        }
        if (measurementTogglePending) {
            close();
            return ;
        }
        if (opened)
            close();
        else
            open();
    }

    function closeForPopoutSwitch() {
        close();
    }

    function toggleMeasurementAndClose() {
        if (measurementTogglePending)
            return ;

        // Leaving measurement mode does not capture the screen and remains an
        // immediate idempotent action.
        if (vernier.measurementActive) {
            close();
            vernier.toggleMeasurement();
            return ;
        }
        openRequestPending = false;
        if (vernier.supportsPreparedActivation) {
            var token = vernier.takePreparedActivationToken(ownerId());
            if (token === "") {
                opened = false;
                return ;
            }
            preparedTokenAfterClose = token;
        } else {
            // Compatibility for older daemons: wait for PopupCard's real
            // fade-complete visibility signal instead of guessing a delay.
            legacyActivationAfterClose = true;
        }
        opened = false;
        if (!popup.visible)
            Qt.callLater(finishMeasurementActionAfterClose);

    }

    function finishMeasurementActionAfterClose() {
        if (popup.visible)
            return ;

        if (preparedTokenAfterClose !== "") {
            var token = preparedTokenAfterClose;
            preparedTokenAfterClose = "";
            vernier.activatePreparedToken(token, ownerId());
        } else if (legacyActivationAfterClose) {
            legacyActivationAfterClose = false;
            vernier.toggleMeasurement();
        }
    }

    Component.onDestruction: {
        // A token still in this property has left the shared service but has
        // not yet been handed to preparedActivationProcess. Cancel exactly
        // that token before the fading widget disappears. Once
        // finishMeasurementActionAfterClose clears it, the activation process
        // owns the token and teardown must not interfere.
        var transferredToken = preparedTokenAfterClose;
        preparedTokenAfterClose = "";
        legacyActivationAfterClose = false;
        quickActivationRequestPending = false;
        if (transferredToken !== "" && vernier && typeof vernier.cancelPreparedActivationToken === "function")
            vernier.cancelPreparedActivationToken(transferredToken);

        if (activationRequestOwner !== "" && vernier && typeof vernier.cancelPreparedActivation === "function")
            vernier.cancelPreparedActivation(activationRequestOwner);

        releasePreparationPopout();
    }
    moduleName: "com.jondkinney.vernier"
    implicitWidth: button.implicitWidth
    implicitHeight: button.implicitHeight

    VernierService {
        id: fallbackVernier

        // The manifest's shared service normally owns all polling. The local
        // instance is an inert fallback until that service becomes available,
        // and remains functional under a minimal standalone QML smoke host.
        enabled: root.sharedVernier === null
    }

    BarIconButton {
        id: button

        anchors.fill: parent
        bar: root.bar
        interactive: !root.activationAfterClosePending && vernier.activationPending !== true
        active: root.openRequestPending || root.quickActivationRequestPending || root.activationRequestError !== "" || vernier.activationError !== "" || vernier.measurementActive || (vernier.checked && !vernier.installed)
        dimmed: vernier.checked && vernier.installed && !vernier.running
        tooltipText: root.activationRequestError || vernier.activationError || root.statusTitle
        onPressed: function(buttonCode) {
            if (buttonCode === Qt.MiddleButton && vernier.running) {
                if (root.opened) {
                    root.toggleMeasurementAndClose();
                } else if (root.openRequestPending || root.quickActivationRequestPending) {
                    root.close();
                } else if (!root.measurementTogglePending) {
                    // Deactivation never captures. Activation can use the
                    // ordinary path only when no popup remains mapped; otherwise
                    // prepare a clean frame while closing the other popout, then
                    // consume it without ever opening Vernier's own panel.
                    if (vernier.measurementActive || !root.hasMappedPopout())
                        vernier.toggleMeasurement();
                    else
                        root.beginQuickActivation();
                }
            } else if (buttonCode === Qt.RightButton && vernier.installed) {
                vernier.openPreferences();
            } else {
                root.toggle();
            }
        }

        iconComponent: Component {
            Item {
                VernierIcon {
                    anchors.centerIn: parent
                    iconSize: Style.space(12)
                    color: root.barIconColor
                    opacity: vernier.checked ? 1 : 0.55
                }

            }

        }

    }

    Connections {
        function onActivationPrepared(ownerId) {
            if (ownerId !== root.ownerId())
                return ;

            if (root.quickActivationRequestPending) {
                root.activationRequestError = "";
                root.quickActivationRequestPending = false;
                var quickToken = root.vernier.takePreparedActivationToken(root.ownerId());
                root.releasePreparationPopout();
                if (quickToken !== "")
                    root.vernier.activatePreparedToken(quickToken, root.ownerId());

                return ;
            }
            if (!root.openRequestPending)
                return ;

            root.activationRequestError = "";
            root.openRequestPending = false;
            root.preparationPopoutClaimed = false;
            root.opened = true;
            root.vernier.refresh();
        }

        function onActivationPreparationFailed(ownerId, message) {
            if (ownerId !== root.ownerId())
                return ;

            root.activationRequestError = message;
            root.openRequestPending = false;
            if (root.quickActivationRequestPending) {
                root.quickActivationRequestPending = false;
                root.releasePreparationPopout();
                return ;
            }
            // Preparation itself failed (for example grim is unavailable).
            // Keep the provisional coordinator claim and hand it to the stock
            // PopupCard lifecycle so status, Preferences, and Quit stay usable;
            // Start remains disabled because there is no prepared token.
            root.preparationPopoutClaimed = false;
            root.opened = true;
        }

        function onActivationPreparationRejected(ownerId, message) {
            if (ownerId !== root.ownerId())
                return ;

            // Ownership rejection is different: another monitor already owns
            // the single prepared frame, so opening here would duplicate UI.
            root.activationRequestError = message;
            root.openRequestPending = false;
            root.quickActivationRequestPending = false;
            root.opened = false;
            root.releasePreparationPopout();
        }

        function onPreparedActivationExpired(ownerId, message) {
            if (ownerId !== root.ownerId())
                return ;

            root.activationRequestError = message;
            root.openRequestPending = false;
            root.quickActivationRequestPending = false;
            root.releasePreparationPopout();
            if (root.opened)
                root.opened = false;

        }

        function onPreparedActivationFailed(ownerId, message) {
            if (ownerId === root.ownerId())
                root.activationRequestError = message;

        }

        target: root.vernier
    }

    PopupCard {
        id: popup

        anchorItem: button
        bar: root.bar
        owner: root
        open: root.opened
        onVisibleChanged: {
            if (!visible)
                root.finishMeasurementActionAfterClose();

        }
        contentWidth: popup.fittedContentWidth(Style.space(310))
        contentHeight: popup.fittedContentHeight(content.implicitHeight, Style.space(520))

        Column {
            id: content

            width: parent.width
            spacing: Style.space(12)

            PanelHero {
                width: parent.width
                title: root.statusTitle
                meta: root.statusMeta
                foreground: root.panelForeground

                trailingControl: Component {
                    Item {
                        visible: vernier.appVersion !== ""
                        implicitWidth: versionLabel.implicitWidth
                        implicitHeight: Style.space(34)

                        Text {
                            id: versionLabel

                            anchors.top: parent.top
                            anchors.right: parent.right
                            text: "v" + vernier.appVersion
                            color: Qt.darker(root.panelForeground, 1.4)
                            font.family: bar ? bar.fontFamily : Style.font.family
                            font.pixelSize: Style.font.body
                            font.bold: true
                        }

                    }

                }

                iconComponent: Component {
                    VernierIcon {
                        iconSize: Style.space(34)
                        color: vernier.measurementActive ? Color.accent : root.panelForeground
                    }

                }

            }

            Text {
                width: parent.width
                visible: !vernier.installed
                text: vernier.installState === "installing" ? "The installer is running in a terminal. Vernier will appear here as soon as it starts." : vernier.installState === "failed" ? "Installation did not complete. Open the installer again to retry." : "Install Vernier from the AUR, then start it under your Omarchy session. The installer stays visible for any prompts."
                color: Qt.darker(root.panelForeground, 1.3)
                font.family: bar ? bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.body
                wrapMode: Text.WordWrap
            }

            Button {
                width: parent.width
                visible: !vernier.installed
                text: vernier.installState === "installing" ? "Installing Vernier…" : "Install Vernier"
                iconText: vernier.installState === "installing" ? "󰑓" : "󰏔"
                iconSpinning: vernier.installState === "installing"
                foreground: root.panelForeground
                accent: Color.accent
                selected: vernier.installState !== "installing"
                bordered: true
                focusable: true
                enabled: vernier.installState !== "installing"
                opacity: enabled ? 1 : 0.65
                onClicked: vernier.install()
            }

            Button {
                width: parent.width
                visible: vernier.installed && !vernier.running
                text: "Start Vernier"
                iconText: "󰐊"
                foreground: root.panelForeground
                accent: Color.accent
                selected: true
                bordered: true
                focusable: true
                onClicked: vernier.start()
            }

            Text {
                width: parent.width
                visible: vernier.running && !vernier.apiSupported
                text: "Update Vernier for live status. Basic toggle, preferences, and quit controls remain available."
                color: Qt.darker(root.panelForeground, 1.3)
                font.family: bar ? bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.body
                wrapMode: Text.WordWrap
            }

            Column {
                width: parent.width
                visible: vernier.running
                spacing: Style.space(10)

                ShortcutButton {
                    width: parent.width
                    text: !vernier.apiSupported ? "Toggle Measuring" : vernier.measurementActive ? "Return to Background" : "Start Measuring"
                    iconText: vernier.measurementActive ? "󰒲" : "󰍉"
                    shortcutText: vernier.apiSupported ? vernier.toggleShortcutHint : ""
                    foreground: root.panelForeground
                    accent: Color.accent
                    selected: vernier.measurementActive
                    bordered: true
                    focusable: true
                    enabled: vernier.measurementActive || !vernier.supportsPreparedActivation || vernier.preparedActivationReady
                    opacity: enabled ? 1 : 0.65
                    onClicked: root.toggleMeasurementAndClose()
                }

                Text {
                    width: parent.width
                    visible: vernier.apiSupported
                    text: vernier.heldRectCount + " held  ·  " + vernier.guideCount + " guides  ·  " + vernier.stuckMeasurementCount + " pinned"
                    color: Qt.darker(root.panelForeground, 1.3)
                    font.family: bar ? bar.fontFamily : Style.font.family
                    font.pixelSize: Style.font.caption
                    horizontalAlignment: Text.AlignHCenter
                    elide: Text.ElideRight
                }

                ShortcutButton {
                    width: parent.width
                    visible: vernier.clearSupported
                    text: vernier.clearPending ? "Clearing…" : "Clear"
                    iconText: vernier.clearPending ? "󰑓" : "󰎟"
                    iconSpinning: vernier.clearPending
                    shortcutText: vernier.clearAndExitShortcutHint
                    tooltipText: shortcutText === "" ? "" : "Shortcut also exits measuring"
                    foreground: root.panelForeground
                    bordered: true
                    focusable: true
                    enabled: vernier.hasClearableContent && !vernier.clearPending
                    opacity: enabled ? 1 : 0.65
                    onClicked: vernier.clearMeasurements()
                }

                PanelSeparator {
                    foreground: root.panelForeground
                }

                Row {
                    width: parent.width
                    spacing: Style.space(8)

                    Button {
                        width: (parent.width - parent.spacing) / 2
                        text: "Preferences"
                        iconText: "󰒓"
                        foreground: root.panelForeground
                        bordered: true
                        focusable: true
                        onClicked: vernier.openPreferences()
                    }

                    Button {
                        width: (parent.width - parent.spacing) / 2
                        text: "Quit"
                        iconText: "󰗼"
                        foreground: Color.urgent
                        bordered: true
                        focusable: true
                        onClicked: {
                            vernier.quit();
                            root.close();
                        }
                    }

                }

            }

            Text {
                width: parent.width
                visible: vernier.actionStatus !== "" || vernier.activationError !== "" || vernier.clearError !== "" || vernier.lastError !== ""
                text: vernier.activationError || vernier.clearError || vernier.lastError || vernier.actionStatus
                color: vernier.activationError || vernier.clearError || vernier.lastError ? Color.urgent : Qt.darker(root.panelForeground, 1.3)
                font.family: bar ? bar.fontFamily : Style.font.family
                font.pixelSize: Style.font.caption
                wrapMode: Text.WordWrap
                horizontalAlignment: Text.AlignHCenter
            }

        }

    }

    component ShortcutButton: Button {
        id: shortcutButton

        property string shortcutText: ""
        readonly property var shortcutTokens: shortcutText === "" ? [] : shortcutText.split(" ")
        readonly property int normalTokenSize: Math.round(Style.font.caption / 0.75)
        readonly property int tokenGap: Math.round(6 * normalTokenSize / 17)

        leftAlign: shortcutText !== ""
        rightPadding: shortcutText !== "" ? horizontalPadding + shortcutLabel.implicitWidth + Style.spacing.controlGap * 2 : horizontalPadding

        Row {
            id: shortcutLabel

            anchors.right: parent.right
            anchors.rightMargin: shortcutButton.horizontalPadding
            anchors.verticalCenter: parent.verticalCenter
            visible: shortcutButton.shortcutText !== ""
            spacing: shortcutButton.tokenGap
            opacity: 0.78

            Repeater {
                model: shortcutButton.shortcutTokens

                Text {
                    required property string modelData

                    anchors.verticalCenter: parent.verticalCenter
                    text: modelData
                    color: shortcutButton.selected ? Style.selectedStateColor(shortcutButton.foreground, shortcutButton.accent) : shortcutButton.foreground
                    // Match the Preferences chip's optical proportions at a
                    // compact 10/13 scale:
                    // ordinary glyphs use Adwaita Sans, while the full-em
                    // Omarchy logo stays caption-sized without synthetic bold.
                    font.family: modelData === "\ue900" ? "omarchy" : "Adwaita Sans"
                    font.pixelSize: modelData === "\ue900" ? Style.font.caption : shortcutButton.normalTokenSize
                    font.bold: false
                }

            }

        }

    }

}
