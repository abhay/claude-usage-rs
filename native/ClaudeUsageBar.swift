import AppKit
import Foundation
import SwiftUI

// This source is embedded in the Rust binary and compiled into a tiny app by
// `claude-usage menubar --install`. Keeping the native shell dependency-free
// lets the CLI remain the single source of truth for usage and loop state.

private enum MenuMode: String, CaseIterable {
    case codex
    case claude
    case work

    var title: String {
        switch self {
        case .codex: "Codex"
        case .claude: "Claude"
        case .work: "Work"
        }
    }

    var symbol: String {
        switch self {
        case .codex: "circle.hexagongrid.fill"
        case .claude: "sparkles"
        case .work: "arrow.triangle.2.circlepath"
        }
    }

    var iconResourceName: String? {
        switch self {
        case .codex: "ProviderIcon-codex"
        case .claude: "ProviderIcon-claude"
        case .work: nil
        }
    }
}

private struct MenuSnapshot: Decodable, Sendable {
    let statusText: String
    let updatedAt: String
    let usage: UsageSnapshot
    let codex: CodexUsageSnapshot
    let loops: [LoopSummary]
    let sessions: [SessionSummary]
    let olderLoopCount: Int
    let awake: AwakeSummary?
    let dashboardRunning: Bool

    static let empty = MenuSnapshot(
        statusText: "",
        updatedAt: "",
        usage: .empty,
        codex: .empty,
        loops: [],
        sessions: [],
        olderLoopCount: 0,
        awake: nil,
        dashboardRunning: false)
}

private struct CodexUsageSnapshot: Decodable, Sendable {
    let available: Bool
    let updatedLabel: String
    let plan: String?
    let windows: [CodexUsageWindow]
    let creditsRemaining: Double?
    let creditsUnlimited: Bool
    let stale: Bool
    let error: String?

    static let empty = CodexUsageSnapshot(
        available: false,
        updatedLabel: "Loading Codex usage…",
        plan: nil,
        windows: [],
        creditsRemaining: nil,
        creditsUnlimited: false,
        stale: false,
        error: nil)
}

private struct CodexUsageWindow: Decodable, Sendable, Identifiable {
    let id: String
    let title: String
    let percent: Double
    let resetLabel: String?
    let additional: Bool

    var meter: UsageMeter {
        UsageMeter(percent: self.percent, resetLabel: self.resetLabel)
    }
}

private struct UsageSnapshot: Decodable, Sendable {
    let model: String?
    let updatedLabel: String
    let fiveHour: UsageMeter?
    let sevenDay: UsageMeter?
    let context: UsageMeter?
    let pace: PaceSummary?
    let activeCostUsd: Double
    let activeSessions: Int
    let hourlyCostUsd: Double?
    let todayTokens: UInt64
    let weekTokens: UInt64
    let promoTitle: String?
    let promoDetail: String?
    let favorable: Bool
    let multiplier: Double
    let apiIndicator: String?
    let apiDescription: String?

    static let empty = UsageSnapshot(
        model: nil,
        updatedLabel: "Waiting for Claude Code",
        fiveHour: nil,
        sevenDay: nil,
        context: nil,
        pace: nil,
        activeCostUsd: 0,
        activeSessions: 0,
        hourlyCostUsd: nil,
        todayTokens: 0,
        weekTokens: 0,
        promoTitle: nil,
        promoDetail: nil,
        favorable: false,
        multiplier: 1,
        apiIndicator: nil,
        apiDescription: nil)
}

private struct UsageMeter: Decodable, Sendable {
    let percent: Double
    let resetLabel: String?
}

private struct PaceSummary: Decodable, Sendable {
    let label: String
    let deltaPercent: Double
}

private struct LoopSummary: Decodable, Sendable, Identifiable {
    let id: String
    let name: String
    let state: String
    let running: Bool
    let stageCurrent: UInt32?
    let stageTotal: UInt32?
    let stageTitle: String?
    let stageDone: Int
    let stageCount: Int
    let taskDone: Int
    let taskCount: Int
    let iteration: UInt64?
    let costUsd: Double?
    let updatedLabel: String
    let lastEvent: String?
}

private struct SessionSummary: Decodable, Sendable, Identifiable {
    let id: String
    let name: String
    let state: String
    let pid: UInt32?
    let goal: String?
    let updatedLabel: String
    let directory: String
    let lastMessage: String?
}

private struct AwakeSummary: Decodable, Sendable {
    let lid: Bool
    let startedLabel: String
    let untilLabel: String?
}

private enum MenuLoadError: LocalizedError {
    case cliMissing
    case commandFailed(String)
    case invalidSnapshot(String)

    var errorDescription: String? {
        switch self {
        case .cliMissing:
            "The claude-usage CLI could not be found. Reinstall the menu app from the CLI."
        case let .commandFailed(message):
            message.isEmpty ? "The claude-usage snapshot command failed." : message
        case let .invalidSnapshot(message):
            "Could not read the claude-usage snapshot: \(message)"
        }
    }
}

private struct SnapshotLoadResult: Sendable {
    let snapshot: MenuSnapshot?
    let error: String?
}

private enum CLIProcess {
    static var executableURL: URL? {
        if let configured = Bundle.main.object(forInfoDictionaryKey: "ClaudeUsageCLIPath") as? String,
           FileManager.default.isExecutableFile(atPath: configured)
        {
            return URL(fileURLWithPath: configured)
        }

        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let candidates = [
            "\(home)/.cargo/bin/claude-usage",
            "/opt/homebrew/bin/claude-usage",
            "/usr/local/bin/claude-usage",
        ]
        return candidates.first(where: FileManager.default.isExecutableFile(atPath:)).map(URL.init(fileURLWithPath:))
    }

    static func loadSnapshot() throws -> MenuSnapshot {
        guard let executableURL else { throw MenuLoadError.cliMissing }
        let process = self.process(executableURL: executableURL, arguments: ["menubar", "--json"])
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr
        do {
            try process.run()
        } catch {
            throw MenuLoadError.commandFailed(error.localizedDescription)
        }
        let data = stdout.fileHandleForReading.readDataToEndOfFile()
        let errorData = stderr.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        if process.terminationStatus != 0 {
            let message = String(
                data: errorData,
                encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            throw MenuLoadError.commandFailed(message)
        }

        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            return try decoder.decode(MenuSnapshot.self, from: data)
        } catch {
            throw MenuLoadError.invalidSnapshot(error.localizedDescription)
        }
    }

    @discardableResult
    static func runAndWait(_ arguments: [String]) -> Bool {
        guard let executableURL else { return false }
        let process = self.process(executableURL: executableURL, arguments: arguments)
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
            return process.terminationStatus == 0
        } catch {
            return false
        }
    }

    static func launch(_ arguments: [String]) {
        guard let executableURL else { return }
        let process = self.process(executableURL: executableURL, arguments: arguments)
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try? process.run()
    }

    private static func process(executableURL: URL, arguments: [String]) -> Process {
        let process = Process()
        process.executableURL = executableURL
        process.arguments = arguments
        var environment = ProcessInfo.processInfo.environment
        let commonPath = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        environment["PATH"] = [environment["PATH"], commonPath].compactMap { $0 }.joined(separator: ":")
        process.environment = environment
        return process
    }
}

@main
private enum ClaudeUsageBarMain {
    @MainActor
    static func main() {
        if let renderIndex = CommandLine.arguments.firstIndex(of: "--render-preview"),
           CommandLine.arguments.indices.contains(renderIndex + 1)
        {
            let path = CommandLine.arguments[renderIndex + 1]
            let mode = CommandLine.arguments
                .first(where: { $0.hasPrefix("--preview-mode=") })
                .flatMap { MenuMode(rawValue: String($0.dropFirst(15))) }
                ?? .claude
            do {
                try self.renderPreview(path: path, mode: mode)
            } catch {
                fputs("Preview rendering failed: \(error.localizedDescription)\n", stderr)
                Foundation.exit(1)
            }
            return
        }

        let application = NSApplication.shared
        let delegate = AppDelegate()
        application.delegate = delegate
        application.setActivationPolicy(.accessory)
        application.run()
    }

    @MainActor
    private static func renderPreview(path: String, mode: MenuMode) throws {
        _ = NSApplication.shared
        let snapshot = try CLIProcess.loadSnapshot()
        let switcher = ModeSwitcherView(selection: mode, snapshot: snapshot) { _ in }
        switcher.frame = NSRect(x: 0, y: 0, width: 382, height: 66)
        switcher.layoutSubtreeIfNeeded()
        switcher.displayIfNeeded()
        guard let representation = switcher.bitmapImageRepForCachingDisplay(in: switcher.bounds) else {
            throw MenuLoadError.invalidSnapshot("AppKit could not rasterize the provider switcher")
        }
        switcher.cacheDisplay(in: switcher.bounds, to: representation)
        let switcherImage = NSImage(size: switcher.bounds.size)
        switcherImage.addRepresentation(representation)

        let renderer = ImageRenderer(content: MenuPreview(
            snapshot: snapshot,
            mode: mode,
            switcherImage: switcherImage))
        renderer.scale = 2
        guard let image = renderer.nsImage,
              let tiff = image.tiffRepresentation,
              let bitmap = NSBitmapImageRep(data: tiff),
              let png = bitmap.representation(using: .png, properties: [:])
        else {
            throw MenuLoadError.invalidSnapshot("AppKit could not rasterize the preview")
        }
        try png.write(to: URL(fileURLWithPath: path), options: .atomic)
    }
}

@MainActor
private final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private static let menuWidth: CGFloat = 382
    private static let panelHeight: CGFloat = 386
    private static let statusItemAutosaveName = "ClaudeUsageStatusItem"
    private static let initialStatusItemPosition: Double = 330
    private static let repairedVisibilityDefaultKey = "hasRepairedClaudeUsageStatusItemVisibility"
    private static let shownTahoeGuidanceKey = "hasShownClaudeUsageTahoeMenuBarGuidance"

    private static func makeStatusItem() -> NSStatusItem {
        let defaults = UserDefaults.standard
        let visibilityKey = "NSStatusItem VisibleCC \(Self.statusItemAutosaveName)"
        let positionKey = "NSStatusItem Preferred Position \(Self.statusItemAutosaveName)"
        if !defaults.bool(forKey: Self.repairedVisibilityDefaultKey),
           let visible = defaults.object(forKey: visibilityKey) as? NSNumber,
           !visible.boolValue
        {
            defaults.removeObject(forKey: visibilityKey)
        }
        defaults.set(true, forKey: Self.repairedVisibilityDefaultKey)
        if defaults.object(forKey: positionKey) == nil {
            // New status items otherwise land at the far-left edge of a crowded
            // menu bar, where macOS hides them behind the active app's menus.
            defaults.set(Self.initialStatusItemPosition, forKey: positionKey)
        }

        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        item.autosaveName = Self.statusItemAutosaveName
        item.isVisible = true
        return item
    }

    private var statusItem = AppDelegate.makeStatusItem()
    private let menu = NSMenu()
    private var snapshot = MenuSnapshot.empty
    private var loadError: String?
    private var selectedMode = MenuMode.claude
    private var contentItem: NSMenuItem?
    private var timer: Timer?
    private var refreshTask: Task<Void, Never>?
    private var menuIsOpen = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        self.configureStatusItem()
        self.menu.delegate = self
        self.menu.autoenablesItems = false
        self.menu.minimumWidth = Self.menuWidth
        self.statusItem.menu = self.menu
        self.rebuildMenu()
        self.refreshSnapshot()
        self.scheduleStatusItemVisibilityCheck()

        if CommandLine.arguments.contains("--open-menu") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) { [weak self] in
                self?.statusItem.button?.performClick(nil)
            }
        }

        let timer = Timer(timeInterval: 60, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.refreshSnapshot()
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func applicationWillTerminate(_ notification: Notification) {
        self.timer?.invalidate()
        self.refreshTask?.cancel()
    }

    func menuWillOpen(_ menu: NSMenu) {
        self.menuIsOpen = true
        self.refreshSnapshot()
        self.rebuildMenu()
    }

    func menuDidClose(_ menu: NSMenu) {
        self.menuIsOpen = false
    }

    private func configureStatusItem() {
        guard let button = self.statusItem.button else { return }
        let image = Bundle.main.url(forResource: "StatusIcon-ai", withExtension: "svg")
            .flatMap(NSImage.init(contentsOf:))
            ?? NSImage(systemSymbolName: "sparkles", accessibilityDescription: "AI Usage")
        image?.size = NSSize(width: 17, height: 17)
        image?.isTemplate = true
        button.image = image
        button.imagePosition = .imageOnly
        button.imageScaling = .scaleProportionallyDown
        button.title = ""
        button.toolTip = "AI Usage"
        button.setAccessibilityTitle("AI Usage")
    }

    private func scheduleStatusItemVisibilityCheck() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
            guard let self, self.statusItemNeedsRecovery else { return }
            self.recreateStatusItem()
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.75) { [weak self] in
                guard let self, self.statusItemNeedsRecovery else { return }
                self.presentTahoeVisibilityGuidanceIfNeeded()
            }
        }
    }

    private var statusItemNeedsRecovery: Bool {
        guard self.statusItem.isVisible, let button = self.statusItem.button else { return true }
        return button.window == nil || button.frame.width <= 0
    }

    private func recreateStatusItem() {
        self.statusItem.menu = nil
        NSStatusBar.system.removeStatusItem(self.statusItem)
        self.statusItem = Self.makeStatusItem()
        self.configureStatusItem()
        self.statusItem.menu = self.menu
        self.updateStatusItem()
    }

    private func presentTahoeVisibilityGuidanceIfNeeded() {
        guard #available(macOS 26.0, *) else { return }
        let defaults = UserDefaults.standard
        guard !defaults.bool(forKey: Self.shownTahoeGuidanceKey) else { return }
        defaults.set(true, forKey: Self.shownTahoeGuidanceKey)

        NSApp.activate(ignoringOtherApps: true)
        let alert = NSAlert()
        alert.messageText = "Claude Usage can't show its menu bar icon"
        alert.informativeText =
            "macOS Tahoe is hiding Claude Usage. Open System Settings → Menu Bar → Allow in the Menu Bar, then turn Claude Usage on."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Open Menu Bar Settings")
        alert.addButton(withTitle: "Dismiss")
        if alert.runModal() == .alertFirstButtonReturn,
           let settingsURL = URL(string: "x-apple.systempreferences:com.apple.MenuBarSettings")
        {
            NSWorkspace.shared.open(settingsURL)
        }
    }

    private func refreshSnapshot() {
        guard self.refreshTask == nil else { return }
        self.refreshTask = Task { [weak self] in
            let result = await Task.detached(priority: .utility) {
                do {
                    return SnapshotLoadResult(snapshot: try CLIProcess.loadSnapshot(), error: nil)
                } catch {
                    return SnapshotLoadResult(snapshot: nil, error: error.localizedDescription)
                }
            }.value
            guard let self else { return }
            if let snapshot = result.snapshot {
                self.snapshot = snapshot
                self.loadError = nil
            } else {
                self.loadError = result.error
            }
            self.refreshTask = nil
            self.updateStatusItem()
            self.rebuildMenu()
        }
    }

    private func updateStatusItem() {
        guard let button = self.statusItem.button else { return }
        button.title = ""
        let running = self.snapshot.loops.filter(\.running).count
        let sessions = self.snapshot.sessions.count
        var tooltip = "\(running) running loop\(running == 1 ? "" : "s") · \(sessions) session\(sessions == 1 ? "" : "s")"
        if !self.snapshot.statusText.isEmpty {
            tooltip = "\(self.snapshot.statusText) · \(tooltip)"
        }
        if self.snapshot.awake != nil {
            tooltip += " · keeping Mac awake"
        }
        button.toolTip = tooltip
    }

    private func rebuildMenu() {
        self.menu.removeAllItems()

        let switcher = ModeSwitcherView(
            selection: self.selectedMode,
            snapshot: self.snapshot)
        { [weak self] mode in
            guard let self else { return }
            self.selectedMode = mode
            self.replaceContentView()
        }
        let switcherItem = NSMenuItem()
        switcherItem.view = switcher
        switcherItem.isEnabled = true
        self.menu.addItem(switcherItem)
        self.menu.addItem(.separator())

        let contentItem = NSMenuItem()
        contentItem.isEnabled = false
        self.contentItem = contentItem
        self.menu.addItem(contentItem)
        self.replaceContentView()

        self.menu.addItem(.separator())
        let awakeTitle = self.snapshot.awake == nil ? "Keep Mac Awake" : "Keeping Mac Awake"
        let awakeItem = self.actionItem(
            title: awakeTitle,
            symbol: self.snapshot.awake == nil ? "moon.zzz" : "cup.and.saucer.fill",
            selector: #selector(self.toggleAwake))
        awakeItem.state = self.snapshot.awake == nil ? .off : .on
        self.menu.addItem(awakeItem)

        self.menu.addItem(self.actionItem(
            title: self.snapshot.dashboardRunning ? "Open Loop Dashboard" : "Start Loop Dashboard",
            symbol: "gauge.with.dots.needle.50percent",
            selector: #selector(self.openDashboard)))
        let refreshItem = self.actionItem(
            title: "Refresh",
            symbol: "arrow.clockwise",
            selector: #selector(self.refreshNow))
        refreshItem.keyEquivalent = "r"
        refreshItem.keyEquivalentModifierMask = [.command]
        self.menu.addItem(refreshItem)

        self.menu.addItem(.separator())
        self.menu.addItem(self.actionItem(
            title: "OpenAI Status",
            symbol: "waveform.path.ecg",
            selector: #selector(self.openOpenAIStatusPage)))
        self.menu.addItem(self.actionItem(
            title: "Anthropic Status",
            symbol: "waveform.path.ecg",
            selector: #selector(self.openAnthropicStatusPage)))
        self.menu.addItem(self.actionItem(
            title: "About Claude Usage",
            symbol: "info.circle",
            selector: #selector(self.openProjectPage)))
        self.menu.addItem(.separator())
        self.menu.addItem(self.actionItem(
            title: "Quit Claude Usage",
            symbol: "power",
            selector: #selector(self.quit)))
    }

    private func replaceContentView() {
        guard let contentItem else { return }
        let root: AnyView = switch self.selectedMode {
        case .codex:
            AnyView(CodexPanel(snapshot: self.snapshot, loadError: self.loadError))
        case .claude:
            AnyView(ClaudePanel(snapshot: self.snapshot, loadError: self.loadError))
        case .work:
            AnyView(WorkPanel(snapshot: self.snapshot, loadError: self.loadError))
        }
        let framed = root.frame(
            width: Self.menuWidth,
            height: Self.panelHeight,
            alignment: .topLeading)
        let host = NSHostingView(rootView: framed)
        host.frame = NSRect(x: 0, y: 0, width: Self.menuWidth, height: Self.panelHeight)
        contentItem.view = host
    }

    private func actionItem(title: String, symbol: String, selector: Selector) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: selector, keyEquivalent: "")
        item.target = self
        item.image = NSImage(systemSymbolName: symbol, accessibilityDescription: title)
        item.image?.isTemplate = true
        item.isEnabled = true
        return item
    }

    @objc private func toggleAwake() {
        if self.snapshot.awake == nil {
            _ = CLIProcess.runAndWait(["awake", "on"])
        } else {
            _ = CLIProcess.runAndWait(["awake", "off"])
        }
        self.refreshSnapshot()
    }

    @objc private func openDashboard() {
        if self.snapshot.dashboardRunning,
           let url = URL(string: "http://127.0.0.1:4711")
        {
            NSWorkspace.shared.open(url)
        } else {
            CLIProcess.launch(["loops", "--serve", "--open"])
        }
    }

    @objc private func refreshNow() {
        self.refreshSnapshot()
    }

    @objc private func openOpenAIStatusPage() {
        if let url = URL(string: "https://status.openai.com") {
            NSWorkspace.shared.open(url)
        }
    }

    @objc private func openAnthropicStatusPage() {
        if let url = URL(string: "https://status.claude.com") {
            NSWorkspace.shared.open(url)
        }
    }

    @objc private func openProjectPage() {
        if let url = URL(string: "https://github.com/abhay/claude-usage-rs") {
            NSWorkspace.shared.open(url)
        }
    }

    @objc private func quit() {
        NSApplication.shared.terminate(nil)
    }
}

private final class ModeSwitcherView: NSView {
    private let modes = MenuMode.allCases
    private let buttons: [ModeTileButton]
    private let onSelect: (MenuMode) -> Void
    private var selectedMode: MenuMode
    private var pressedIndex: Int?
    private var hoveredIndex: Int?
    private var trackingArea: NSTrackingArea?

    init(selection: MenuMode, snapshot: MenuSnapshot, onSelect: @escaping (MenuMode) -> Void) {
        self.selectedMode = selection
        self.onSelect = onSelect

        let claudePercent = snapshot.usage.sevenDay?.percent ?? snapshot.usage.fiveHour?.percent ?? 0
        let codexPercent = snapshot.codex.windows.map(\.percent).max() ?? 0
        let runningLoops = snapshot.loops.filter(\.running).count
        let busySessions = snapshot.sessions.filter { $0.state == "busy" }.count
        let workItems = snapshot.loops.count + snapshot.sessions.count
        let activeWorkItems = runningLoops + busySessions
        let progress = [
            min(max(codexPercent / 100, 0), 1),
            min(max(claudePercent / 100, 0), 1),
            workItems == 0 ? 0 : Double(activeWorkItems) / Double(workItems),
        ]
        let colors = [NSColor.systemBlue, NSColor.systemOrange, NSColor.systemIndigo]
        self.buttons = zip(MenuMode.allCases.indices, MenuMode.allCases).map { index, mode in
            ModeTileButton(mode: mode, progress: progress[index], accent: colors[index])
        }

        super.init(frame: NSRect(x: 0, y: 0, width: 382, height: 66))
        self.wantsLayer = true
        for button in self.buttons {
            self.addSubview(button)
        }
        self.updateStyles()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func layout() {
        super.layout()
        let horizontalInset: CGFloat = 8
        let gap: CGFloat = 4
        let width = (self.bounds.width - horizontalInset * 2 - gap * 2) / 3
        for (index, button) in self.buttons.enumerated() {
            button.frame = NSRect(
                x: horizontalInset + CGFloat(index) * (width + gap),
                y: 5,
                width: width,
                height: self.bounds.height - 10)
        }
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool {
        true
    }

    override func hitTest(_ point: NSPoint) -> NSView? {
        self.bounds.contains(point) ? self : nil
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let trackingArea {
            self.removeTrackingArea(trackingArea)
        }
        let area = NSTrackingArea(
            rect: .zero,
            options: [.activeAlways, .inVisibleRect, .mouseMoved, .mouseEnteredAndExited],
            owner: self,
            userInfo: nil)
        self.addTrackingArea(area)
        self.trackingArea = area
    }

    override func mouseMoved(with event: NSEvent) {
        let index = self.index(at: self.convert(event.locationInWindow, from: nil))
        guard index != self.hoveredIndex else { return }
        self.hoveredIndex = index
        self.updateStyles()
    }

    override func mouseExited(with event: NSEvent) {
        self.hoveredIndex = nil
        self.updateStyles()
    }

    override func mouseDown(with event: NSEvent) {
        self.pressedIndex = self.index(at: self.convert(event.locationInWindow, from: nil))
    }

    override func mouseUp(with event: NSEvent) {
        defer { self.pressedIndex = nil }
        let released = self.index(at: self.convert(event.locationInWindow, from: nil))
        guard let pressedIndex, released == pressedIndex, self.modes.indices.contains(pressedIndex) else { return }
        let mode = self.modes[pressedIndex]
        guard mode != self.selectedMode else { return }
        self.selectedMode = mode
        self.updateStyles()
        self.onSelect(mode)
    }

    private func index(at point: NSPoint) -> Int? {
        self.buttons.firstIndex(where: { $0.frame.contains(point) })
    }

    private func updateStyles() {
        for (index, button) in self.buttons.enumerated() {
            button.updateStyle(
                selected: button.mode == self.selectedMode,
                hovered: index == self.hoveredIndex)
        }
    }
}

private final class ModeTileButton: NSView {
    let mode: MenuMode
    private let progress: CGFloat
    private let accent: NSColor
    private let iconView = NSImageView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let trackView = NSView()
    private let fillView = NSView()

    override var isFlipped: Bool { true }

    init(mode: MenuMode, progress: Double, accent: NSColor) {
        self.mode = mode
        self.progress = CGFloat(progress)
        self.accent = accent
        super.init(frame: .zero)

        let image: NSImage? = if let resource = mode.iconResourceName,
                                 let url = Bundle.main.url(forResource: resource, withExtension: "svg")
        {
            NSImage(contentsOf: url)
        } else {
            NSImage(systemSymbolName: mode.symbol, accessibilityDescription: mode.title)?
                .withSymbolConfiguration(.init(pointSize: 16, weight: .medium))
        }
        image?.isTemplate = true
        self.iconView.image = image
        self.iconView.imageScaling = .scaleProportionallyUpOrDown
        self.titleLabel.stringValue = mode.title
        self.titleLabel.alignment = .center
        self.titleLabel.lineBreakMode = .byTruncatingTail
        self.wantsLayer = true
        self.layer?.cornerRadius = 9
        self.layer?.masksToBounds = true
        self.trackView.wantsLayer = true
        self.trackView.layer?.cornerRadius = 1.5
        self.fillView.wantsLayer = true
        self.fillView.layer?.cornerRadius = 1.5
        self.trackView.addSubview(self.fillView)
        self.addSubview(self.iconView)
        self.addSubview(self.titleLabel)
        self.addSubview(self.trackView)
        self.setAccessibilityLabel(mode.title)
        self.setAccessibilityRole(.button)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func layout() {
        super.layout()
        let iconSize: CGFloat = 18
        self.iconView.frame = NSRect(
            x: (self.bounds.width - iconSize) / 2,
            y: 7,
            width: iconSize,
            height: iconSize)
        self.titleLabel.frame = NSRect(x: 6, y: 28, width: self.bounds.width - 12, height: 16)
        self.trackView.frame = NSRect(x: 9, y: self.bounds.height - 8, width: self.bounds.width - 18, height: 3)
        self.fillView.frame = NSRect(
            x: 0,
            y: 0,
            width: self.trackView.bounds.width * min(max(self.progress, 0), 1),
            height: 3)
    }

    func updateStyle(selected: Bool, hovered: Bool) {
        let foreground = selected ? NSColor.white : NSColor.secondaryLabelColor
        self.iconView.contentTintColor = foreground
        self.titleLabel.textColor = foreground
        self.titleLabel.font = .systemFont(ofSize: 11, weight: selected ? .semibold : .medium)
        if selected {
            self.layer?.backgroundColor = NSColor.controlAccentColor.cgColor
            self.trackView.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.26).cgColor
            self.fillView.layer?.backgroundColor = NSColor.white.withAlphaComponent(0.9).cgColor
        } else {
            self.layer?.backgroundColor = hovered
                ? NSColor.labelColor.withAlphaComponent(0.08).cgColor
                : NSColor.clear.cgColor
            self.trackView.layer?.backgroundColor = NSColor.separatorColor.withAlphaComponent(0.5).cgColor
            self.fillView.layer?.backgroundColor = self.accent.cgColor
        }
    }
}

private struct MenuPreview: View {
    let snapshot: MenuSnapshot
    let mode: MenuMode
    let switcherImage: NSImage

    var body: some View {
        ZStack {
            LinearGradient(
                colors: [
                    Color(red: 0.19, green: 0.16, blue: 0.68),
                    Color(red: 0.52, green: 0.28, blue: 0.76),
                    Color(red: 0.14, green: 0.42, blue: 0.82),
                ],
                startPoint: .topLeading,
                endPoint: .bottomTrailing)

            VStack(spacing: 0) {
                Image(nsImage: self.switcherImage)
                    .resizable()
                    .frame(width: 382, height: 66)
                Divider()
                self.panel
                    .frame(width: 382, height: 386, alignment: .topLeading)
                Divider()
                PreviewActionRow(
                    symbol: self.snapshot.awake == nil ? "moon.zzz" : "cup.and.saucer.fill",
                    title: self.snapshot.awake == nil ? "Keep Mac Awake" : "Keeping Mac Awake",
                    checked: self.snapshot.awake != nil)
                PreviewActionRow(
                    symbol: "gauge.with.dots.needle.50percent",
                    title: self.snapshot.dashboardRunning ? "Open Loop Dashboard" : "Start Loop Dashboard")
                PreviewActionRow(symbol: "arrow.clockwise", title: "Refresh", shortcut: "⌘R")
                Divider().padding(.horizontal, 12)
                PreviewActionRow(symbol: "waveform.path.ecg", title: "OpenAI Status")
                PreviewActionRow(symbol: "waveform.path.ecg", title: "Anthropic Status")
                PreviewActionRow(symbol: "info.circle", title: "About Claude Usage")
                Divider().padding(.horizontal, 12)
                PreviewActionRow(symbol: "power", title: "Quit Claude Usage")
            }
            .frame(width: 382)
            .background(
                LinearGradient(
                    colors: [
                        Color(red: 0.76, green: 0.69, blue: 0.95),
                        Color(red: 0.68, green: 0.73, blue: 0.95),
                    ],
                    startPoint: .topLeading,
                    endPoint: .bottomTrailing))
            .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: 18, style: .continuous)
                    .stroke(Color.white.opacity(0.35), lineWidth: 0.75)
            }
            .shadow(color: .black.opacity(0.3), radius: 28, y: 16)
        }
        .frame(width: 470, height: 740)
    }

    @ViewBuilder
    private var panel: some View {
        switch self.mode {
        case .codex:
            CodexPanel(snapshot: self.snapshot, loadError: nil)
        case .claude:
            ClaudePanel(snapshot: self.snapshot, loadError: nil)
        case .work:
            WorkPanel(snapshot: self.snapshot, loadError: nil)
        }
    }
}

private struct PreviewActionRow: View {
    let symbol: String
    let title: String
    var shortcut: String?
    var checked = false

    var body: some View {
        HStack(spacing: 9) {
            Image(systemName: self.checked ? "checkmark" : self.symbol)
                .frame(width: 17)
            Text(self.title)
            Spacer()
            if let shortcut {
                Text(shortcut).foregroundStyle(.secondary)
            }
        }
        .font(.body)
        .padding(.horizontal, 16)
        .frame(height: 29)
    }
}

private struct CodexPanel: View {
    let snapshot: MenuSnapshot
    let loadError: String?

    private var usage: CodexUsageSnapshot { self.snapshot.codex }

    private var planBadge: String? {
        guard let plan = self.usage.plan, !plan.isEmpty else { return nil }
        return plan.prefix(1).uppercased() + plan.dropFirst()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            PanelHeader(
                title: "Codex",
                subtitle: self.usage.updatedLabel,
                badge: self.planBadge,
                badgePositive: self.usage.available && !self.usage.stale)
            Divider()

            if let loadError {
                ErrorCallout(message: loadError)
            } else if !self.usage.available {
                EmptyCallout(
                    symbol: "terminal",
                    title: "Codex usage is unavailable",
                    detail: self.usage.error ?? "Install and sign in to the Codex CLI to load account limits.")
            } else if self.usage.windows.isEmpty {
                EmptyCallout(
                    symbol: "chart.bar.xaxis",
                    title: "No quota windows returned",
                    detail: "The Codex CLI recognized the account, but it did not publish a usage window.")
            } else {
                ForEach(Array(self.usage.windows.prefix(3))) { window in
                    UsageMetric(
                        title: window.title,
                        meter: window.meter,
                        tint: window.additional ? .codexPurple : .codexBlue)
                }
            }

            if self.usage.creditsUnlimited {
                Label("Extra usage · Unlimited", systemImage: "infinity")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if let credits = self.usage.creditsRemaining, credits > 0 {
                Label(
                    "Extra usage · \(credits.formatted(.number.precision(.fractionLength(0 ... 2)))) credits",
                    systemImage: "creditcard")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Spacer(minLength: 0)

            if self.usage.stale, let error = self.usage.error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption2)
                    .foregroundStyle(.orange)
                    .lineLimit(2)
            } else {
                Label("Live from Codex CLI app-server", systemImage: "checkmark.circle.fill")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }
}

private struct ClaudePanel: View {
    let snapshot: MenuSnapshot
    let loadError: String?

    private var usage: UsageSnapshot { self.snapshot.usage }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            PanelHeader(
                title: "Claude",
                subtitle: self.usage.updatedLabel,
                badge: self.usage.promoTitle,
                badgePositive: self.usage.favorable)
            Divider()

            if let loadError {
                ErrorCallout(message: loadError)
            } else if self.usage.fiveHour == nil, self.usage.sevenDay == nil, self.usage.context == nil {
                EmptyCallout(
                    symbol: "chart.bar.xaxis",
                    title: "Usage data is warming up",
                    detail: "Run a Claude Code turn with the statusline enabled to populate live limits.")
            } else {
                if let meter = self.usage.fiveHour {
                    UsageMetric(title: "Session", meter: meter, tint: .usageOrange)
                }
                if let meter = self.usage.sevenDay {
                    UsageMetric(title: "Weekly", meter: meter, tint: .usageOrange)
                }
                if let pace = self.usage.pace {
                    Text("Pace: \(pace.label) (\(pace.deltaPercent.formatted(.number.precision(.fractionLength(0)).sign(strategy: .always())))%)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if let meter = self.usage.context {
                    UsageMetric(title: self.usage.model ?? "Context", meter: meter, tint: .indigo)
                }
            }

            Spacer(minLength: 0)
            ActiveWorkSummary(snapshot: self.snapshot)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }
}

private struct WorkPanel: View {
    let snapshot: MenuSnapshot
    let loadError: String?

    private var runningCount: Int { self.snapshot.loops.filter(\.running).count }
    private var busyCount: Int { self.snapshot.sessions.filter { $0.state == "busy" }.count }
    private var activeCount: Int { self.runningCount + self.busyCount }

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            PanelHeader(
                title: "Active Work",
                subtitle: "\(self.snapshot.loops.count) loops · \(self.snapshot.sessions.count) sessions",
                badge: self.activeCount == 0 ? "Idle" : "\(self.activeCount) active",
                badgePositive: self.activeCount > 0)
            Divider()

            if let loadError {
                ErrorCallout(message: loadError)
            } else {
                VStack(alignment: .leading, spacing: 8) {
                    WorkSectionHeader(
                        title: "Ralph Loops",
                        symbol: "arrow.triangle.2.circlepath",
                        count: self.snapshot.loops.count)
                    if self.snapshot.loops.isEmpty {
                        Text("No recent loops")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(Array(self.snapshot.loops.prefix(2)).indices, id: \.self) { index in
                            LoopRow(loop: self.snapshot.loops[index])
                            if index < min(self.snapshot.loops.count, 2) - 1 {
                                Divider().padding(.leading, 14)
                            }
                        }
                        let hidden = max(0, self.snapshot.loops.count - 2) + self.snapshot.olderLoopCount
                        if hidden > 0 {
                            Text("\(hidden) more in the Loop Dashboard")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .trailing)
                        }
                    }

                    Divider()
                    WorkSectionHeader(
                        title: "Claude Sessions",
                        symbol: "bubble.left.and.bubble.right",
                        count: self.snapshot.sessions.count)
                    if self.snapshot.sessions.isEmpty {
                        Text("No live sessions")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(Array(self.snapshot.sessions.prefix(3)).indices, id: \.self) { index in
                            SessionRow(session: self.snapshot.sessions[index])
                            if index < min(self.snapshot.sessions.count, 3) - 1 {
                                Divider().padding(.leading, 24)
                            }
                        }
                        if self.snapshot.sessions.count > 3 {
                            Text("\(self.snapshot.sessions.count - 3) more in the Loop Dashboard")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .trailing)
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }
}

private struct WorkSectionHeader: View {
    let title: String
    let symbol: String
    let count: Int

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: self.symbol)
                .frame(width: 14)
            Text(self.title)
                .font(.subheadline.weight(.semibold))
            Spacer()
            Text("\(self.count)")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
        }
    }
}

private struct PanelHeader: View {
    let title: String
    let subtitle: String
    let badge: String?
    let badgePositive: Bool

    var body: some View {
        HStack(alignment: .center, spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                Text(self.title)
                    .font(.title3.weight(.semibold))
                Text(self.subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if let badge {
                Text(badge)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(self.badgePositive ? Color.green : Color.secondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(
                        Capsule(style: .continuous)
                            .fill((self.badgePositive ? Color.green : Color.secondary).opacity(0.11)))
            }
        }
    }
}

private struct UsageMetric: View {
    let title: String
    let meter: UsageMeter
    let tint: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(alignment: .firstTextBaseline) {
                Text(self.title)
                    .font(.body.weight(.medium))
                Spacer()
                if let reset = self.meter.resetLabel {
                    Text(reset)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            MeterBar(percent: self.meter.percent, tint: self.tint)
            Text("\(self.meter.percent.formatted(.number.precision(.fractionLength(0))))% used")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

private struct MeterBar: View {
    let percent: Double
    let tint: Color

    var body: some View {
        GeometryReader { geometry in
            let ratio = min(max(self.percent / 100, 0), 1)
            ZStack(alignment: .leading) {
                Capsule(style: .continuous)
                    .fill(Color.secondary.opacity(0.14))
                if ratio > 0 {
                    Capsule(style: .continuous)
                        .fill(self.tint)
                        .frame(width: max(6, geometry.size.width * ratio))
                }
            }
        }
        .frame(height: 6)
        .accessibilityLabel("\(self.percent.formatted(.number.precision(.fractionLength(0)))) percent used")
    }
}

private struct ActiveWorkSummary: View {
    let snapshot: MenuSnapshot

    private var runningLoops: [LoopSummary] { self.snapshot.loops.filter(\.running) }

    private var activeCost: String {
        let cost = self.snapshot.usage.activeCostUsd
        if cost <= 0 { return "—" }
        if cost < 10 { return String(format: "$%.2f", cost) }
        if cost < 1000 { return String(format: "$%.0f", cost) }
        return String(format: "$%.1fK", cost / 1000)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Label("Active work", systemImage: "bolt.horizontal.circle.fill")
                    .font(.subheadline.weight(.semibold))
                Spacer()
                Text(self.snapshot.awake == nil ? "Sleep allowed" : "Mac stays awake")
                    .font(.caption)
                    .foregroundStyle(self.snapshot.awake == nil ? Color.secondary : Color.green)
            }

            HStack(spacing: 8) {
                StatChip(value: "\(self.runningLoops.count)", label: "loops", color: .indigo)
                StatChip(value: "\(self.snapshot.sessions.count)", label: "sessions", color: .teal)
                StatChip(
                    value: self.activeCost,
                    label: "cost",
                    color: .orange)
            }

            if let loop = self.runningLoops.first {
                Text("Now: \(loop.name) · \(loop.stageTitle ?? loop.state.capitalized)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            } else {
                Text("No agent loop is consuming quota right now.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Color.primary.opacity(0.055)))
    }
}

private struct StatChip: View {
    let value: String
    let label: String
    let color: Color

    var body: some View {
        HStack(spacing: 5) {
            Circle().fill(self.color).frame(width: 6, height: 6)
            Text(self.value).font(.caption.weight(.semibold))
            Text(self.label).font(.caption).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 5)
        .background(Capsule(style: .continuous).fill(Color.primary.opacity(0.045)))
    }
}

private struct LoopRow: View {
    let loop: LoopSummary

    private var stateColor: Color {
        if self.loop.running { return .indigo }
        switch self.loop.state {
        case "completed": return .green
        case "failed": return .red
        default: return .secondary
        }
    }

    private var progress: Double {
        guard self.loop.stageCount > 0 else { return 0 }
        return Double(self.loop.stageDone) / Double(self.loop.stageCount) * 100
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 7) {
                Circle().fill(self.stateColor).frame(width: 7, height: 7)
                Text(self.loop.name)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                Spacer()
                Text(self.loop.updatedLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            HStack {
                Text(self.loop.stageTitle ?? self.loop.state.capitalized)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Spacer()
                if let current = self.loop.stageCurrent, let total = self.loop.stageTotal {
                    Text("Stage \(current)/\(total)")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
            }
            MeterBar(percent: self.progress, tint: self.stateColor)
            HStack {
                if self.loop.taskCount > 0 {
                    Text("\(self.loop.taskDone)/\(self.loop.taskCount) tasks")
                }
                if let iteration = self.loop.iteration {
                    Text("Iteration \(iteration)")
                }
                if let cost = self.loop.costUsd, cost > 0 {
                    Text(String(format: "$%.2f", cost))
                }
                Spacer()
            }
            .font(.caption2)
            .foregroundStyle(.tertiary)
        }
    }
}

private struct SessionRow: View {
    let session: SessionSummary

    private var stateColor: Color {
        switch self.session.state {
        case "busy": .teal
        case "shell": .orange
        default: .secondary
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 9) {
            Image(systemName: self.session.state == "shell" ? "terminal" : "bubble.left.fill")
                .foregroundStyle(self.stateColor)
                .frame(width: 15)
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(self.session.name)
                        .font(.subheadline.weight(.semibold))
                        .lineLimit(1)
                    Spacer()
                    Text(self.session.updatedLabel)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                Text(self.session.goal.map { "Goal: \($0)" } ?? self.session.lastMessage ?? self.session.directory)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
    }
}

private struct EmptyCallout: View {
    let symbol: String
    let title: String
    let detail: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: self.symbol)
                .font(.title3)
                .foregroundStyle(.secondary)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 3) {
                Text(self.title).font(.subheadline.weight(.semibold))
                Text(self.detail).font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 11, style: .continuous)
                .fill(Color.primary.opacity(0.05)))
    }
}

private struct ErrorCallout: View {
    let message: String

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(self.message)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .fill(Color.orange.opacity(0.09)))
    }
}

private extension Color {
    static let usageOrange = Color(red: 0.82, green: 0.43, blue: 0.24)
    static let codexBlue = Color(red: 0.12, green: 0.48, blue: 0.94)
    static let codexPurple = Color(red: 0.43, green: 0.32, blue: 0.95)
}
