// Shared by Leaf's native pane helpers (src/browser.swift, src/simulator.swift):
// a borderless window kept exactly over one Rex block.
//
// Rex draws its blocks itself and has no way to host a native view, so a
// helper owns a window and follows the block: the session layout (fractions of
// the window) comes from `rex api call session.view`, refreshed whenever the
// session emits an event; the Rex window's frame comes from the window server
// every frame; the space Rex's own chrome takes (sidebar, toolbar, gaps,
// headers) comes from Rex's preferences. The window sits directly above the
// Rex window at the normal level, so other apps still cover both.
//
// Environment: REX_SESSION, REX_BLOCK (set by Rex in the pane), LEAF_REX (the
// rex program).

import AppKit

// MARK: - Rex

enum Rex {
    static let program = ProcessInfo.processInfo.environment["LEAF_REX"] ?? "rex"
    static let queue = DispatchQueue(label: "leaf.pane.rex", attributes: .concurrent)

    @discardableResult
    static func run(_ arguments: [String]) -> Data? {
        let task = Process()
        if program.contains("/") {
            task.executableURL = URL(fileURLWithPath: program)
            task.arguments = arguments
        } else {
            task.executableURL = URL(fileURLWithPath: "/usr/bin/env")
            task.arguments = [program] + arguments
        }
        let pipe = Pipe()
        task.standardOutput = pipe
        task.standardError = FileHandle.nullDevice
        task.standardInput = FileHandle.nullDevice
        do { try task.run() } catch { return nil }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        task.waitUntilExit()
        return task.terminationStatus == 0 ? data : nil
    }

    static func call(_ method: String, _ input: [String: Any]) -> [String: Any]? {
        guard let body = try? JSONSerialization.data(withJSONObject: input),
              let text = String(data: body, encoding: .utf8),
              let data = run(["api", "call", method, text]) else { return nil }
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }

    static func background(_ work: @escaping () -> Void) { queue.async(execute: work) }
}

/// Where a block sits in the active window, as fractions of the pane area.
struct Placement: Equatable {
    let rect: CGRect
    let focused: Bool
}

enum SessionView {
    static func rect(_ value: Any?) -> CGRect? {
        guard let numbers = value as? [NSNumber], numbers.count == 4 else { return nil }
        return CGRect(x: numbers[0].doubleValue, y: numbers[1].doubleValue,
                      width: numbers[2].doubleValue, height: numbers[3].doubleValue)
    }

    static func find(_ block: String, in node: Any?) -> CGRect? {
        if let object = node as? [String: Any] {
            if object["block_id"] as? String == block { return rect(object["rect"]) }
            for child in object.values { if let found = find(block, in: child) { return found } }
        } else if let array = node as? [Any] {
            for child in array { if let found = find(block, in: child) { return found } }
        }
        return nil
    }

    static func contains(_ view: [String: Any], block: String) -> Bool {
        for window in view["windows"] as? [[String: Any]] ?? [] {
            for layer in window["layers"] as? [[String: Any]] ?? [] {
                for item in layer["blocks"] as? [[String: Any]] ?? [] where item["block_id"] as? String == block {
                    return true
                }
                // The layout tree names every placed block too; checked as
                // well so a view without `blocks` never closes the pane.
                if find(block, in: layer["layout"]) != nil { return true }
            }
        }
        return false
    }

    /// The block's place when it's drawn in a tiled layout of the active
    /// window; nil when another tab is showing, another block is zoomed, or
    /// the block floats (Leaf opens its panes as splits).
    static func placement(_ view: [String: Any], block: String) -> Placement? {
        guard let active = view["active_window_id"] as? String,
              let window = (view["windows"] as? [[String: Any]])?.first(where: { $0["window_id"] as? String == active })
        else { return nil }
        let focused = window["focused_block_id"] as? String == block
        if let zoomed = window["zoomed_block_id"] as? String, !zoomed.isEmpty {
            return zoomed == block ? Placement(rect: CGRect(x: 0, y: 0, width: 1, height: 1), focused: focused) : nil
        }
        for layer in window["layers"] as? [[String: Any]] ?? [] where layer["kind"] as? String == "tiled" {
            if let rect = find(block, in: layer["layout"]) { return Placement(rect: rect, focused: focused) }
        }
        return nil
    }
}

/// The space Rex's window chrome takes around and between tiled panes, read
/// from Rex's own preferences so it tracks the person's settings.
struct Chrome: Equatable {
    var top: CGFloat, left: CGFloat, right: CGFloat, bottom: CGFloat
    var gap: CGFloat, header: CGFloat, radius: CGFloat

    static let domain = "com.superlogical.rex" as CFString

    static func setting(_ key: String) -> Any? {
        CFPreferencesCopyAppValue("Workspace.\(key)" as CFString, domain)
    }

    static func current() -> Chrome {
        CFPreferencesAppSynchronize(domain)
        let compact = setting("paneDensity") as? String == "compact"
        let edge: CGFloat = compact ? 0 : 8
        var chrome = Chrome(top: compact ? 41 : 46, left: edge, right: edge, bottom: edge,
                            gap: compact ? 2 : 8, header: compact ? 35 : 36, radius: compact ? 0 : 10)
        let headers = setting("paneHeaderVisibility") as? String
            ?? (((setting("showsPaneHeaders") as? NSNumber)?.boolValue ?? true) ? "always" : "never")
        if headers != "always" { chrome.header = 0 }
        let vertical = setting("tabStyle") as? String == "vertical"
        let toolbar = setting("toolbarBehavior") as? String ?? "alwaysShow"
        if vertical || toolbar != "alwaysShow" { chrome.top = edge }
        if vertical, !((setting("sidebarCollapsed") as? NSNumber)?.boolValue ?? false) {
            let width = CGFloat((setting("sidebarWidth") as? NSNumber)?.doubleValue ?? 240)
            if setting("sidebarPosition") as? String == "right" { chrome.right = width } else { chrome.left = width }
        }
        return chrome
    }

    /// Rex's light or dark choice; nil when it follows macOS.
    static func appearance() -> NSAppearance? {
        CFPreferencesAppSynchronize(domain)
        switch setting("appearance") as? String {
        case "light": return NSAppearance(named: .aqua)
        case "dark": return NSAppearance(named: .darkAqua)
        default: return nil
        }
    }

    /// The block's body in screen points (top-left origin), inside `window`.
    func body(_ fraction: CGRect, in window: CGRect) -> CGRect {
        let area = CGRect(x: window.minX + left, y: window.minY + top,
                          width: window.width - left - right, height: window.height - top - bottom)
        var card = CGRect(x: area.minX + fraction.minX * area.width, y: area.minY + fraction.minY * area.height,
                          width: fraction.width * area.width, height: fraction.height * area.height)
        let half = gap / 2, edge = 0.001
        if fraction.minX > edge { card.origin.x += half; card.size.width -= half }
        if fraction.maxX < 1 - edge { card.size.width -= half }
        if fraction.minY > edge { card.origin.y += half; card.size.height -= half }
        if fraction.maxY < 1 - edge { card.size.height -= half }
        card.origin.y += header
        card.size.height -= header
        return card
    }
}

struct RexWindow {
    let number: CGWindowID
    let pid: pid_t
    let bounds: CGRect
}

enum WindowServer {
    /// On-screen windows front to back.
    static func list() -> [[String: Any]] {
        CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    }

    static func rexWindows(_ list: [[String: Any]]) -> [RexWindow] {
        list.compactMap { item in
            guard let owner = item[kCGWindowOwnerName as String] as? String,
                  owner == "Rex" || owner.hasPrefix("Rex "),
                  item[kCGWindowLayer as String] as? Int == 0,
                  let number = item[kCGWindowNumber as String] as? CGWindowID,
                  let pid = item[kCGWindowOwnerPID as String] as? pid_t,
                  let dictionary = item[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: dictionary),
                  bounds.width > 200, bounds.height > 150 else { return nil }
            return RexWindow(number: number, pid: pid, bounds: bounds)
        }
    }

    /// One window's current bounds, if it's still on screen.
    static func bounds(of number: CGWindowID) -> CGRect? {
        let info = CGWindowListCopyWindowInfo([.optionIncludingWindow], number) as? [[String: Any]] ?? []
        guard let item = info.first, (item[kCGWindowIsOnscreen as String] as? Bool) ?? false,
              let dictionary = item[kCGWindowBounds as String] as? NSDictionary else { return nil }
        return CGRect(dictionaryRepresentation: dictionary)
    }
}

// MARK: - Rex key bindings

/// A key chord as Rex writes it, such as "shift+cmd+p".
struct Chord: Hashable {
    let modifiers: NSEvent.ModifierFlags.RawValue
    let key: String

    static let relevant: NSEvent.ModifierFlags = [.command, .shift, .option, .control]

    init?(_ text: String) {
        var flags: NSEvent.ModifierFlags = []
        var key: String?
        for part in text.lowercased().split(separator: "+", omittingEmptySubsequences: false) {
            switch part {
            case "cmd", "command", "super": flags.insert(.command)
            case "shift": flags.insert(.shift)
            case "alt", "opt", "option": flags.insert(.option)
            case "ctrl", "control": flags.insert(.control)
            case "": key = "+"
            default: key = String(part)
            }
        }
        guard let key else { return nil }
        modifiers = flags.rawValue
        self.key = key
    }

    init?(_ event: NSEvent) {
        let flags = event.modifierFlags.intersection(Chord.relevant)
        let special: [UInt16: String] = [
            123: "left", 124: "right", 125: "down", 126: "up", 49: "space", 36: "enter", 76: "enter",
            48: "tab", 53: "escape", 51: "backspace", 117: "delete", 115: "home", 119: "end",
            116: "pageup", 121: "pagedown",
        ]
        if let name = special[event.keyCode] {
            key = name
        } else if let base = event.charactersIgnoringModifiers?.lowercased(), !base.isEmpty {
            key = base
        } else {
            return nil
        }
        modifiers = flags.rawValue
    }
}

struct Binding {
    let keys: [Chord]
    let action: String
    let args: Any?
}

// MARK: - Views

/// A borderless symbol button that shows a soft fill under the pointer.
final class ToolButton: NSButton {
    private var hovering = false { didSet { needsDisplay = true } }

    init(symbol: String, label: String, target: AnyObject?, action: Selector) {
        super.init(frame: .zero)
        image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)?
            .withSymbolConfiguration(.init(pointSize: 13, weight: .medium))
        imagePosition = .imageOnly
        isBordered = false
        toolTip = label
        self.target = target
        self.action = action
        contentTintColor = .secondaryLabelColor
        setButtonType(.momentaryChange)
    }

    required init?(coder: NSCoder) { nil }

    func setSymbol(_ symbol: String, label: String) {
        image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)?
            .withSymbolConfiguration(.init(pointSize: 13, weight: .medium))
        toolTip = label
    }

    override var isEnabled: Bool {
        didSet { contentTintColor = isEnabled ? .secondaryLabelColor : .tertiaryLabelColor }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        trackingAreas.forEach(removeTrackingArea)
        addTrackingArea(NSTrackingArea(rect: bounds, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                                       owner: self))
    }

    override func mouseEntered(with event: NSEvent) { hovering = true }
    override func mouseExited(with event: NSEvent) { hovering = false }

    override func draw(_ dirtyRect: NSRect) {
        if hovering && isEnabled {
            NSColor.labelColor.withAlphaComponent(0.08).setFill()
            NSBezierPath(roundedRect: bounds, xRadius: 6, yRadius: 6).fill()
        }
        super.draw(dirtyRect)
    }
}

final class Bar: NSView {
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        NSColor.labelColor.withAlphaComponent(0.08).setFill()
        NSRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1).fill()
    }
}

final class PaneWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}


// MARK: - Pane controller

/// Keeps `window` over one Rex block, hands focus back and forth with Rex,
/// and forwards Rex's own shortcuts. A pane helper subclasses it and fills
/// `root`.
class PaneController: NSObject {
    let session: String
    let block: String
    let window: PaneWindow
    let root = NSView()

    // Layout tracking
    var chrome = Chrome.current()
    var view: [String: Any]?
    var placement: Placement?
    var rex: RexWindow?
    var shown = false
    var lastFrame = NSRect.zero
    var settleUntil = Date.distantPast
    var missing = 0
    var refreshing = false
    var refreshAgain = false
    var events: Process?
    let parent = getppid()

    // Focus and keys
    var wasFocusedInRex = false
    var appClient: String?
    var bindings: [Binding] = []
    var pendingPrefix: [Chord] = []
    var forwardedAt = Date.distantPast
    /// Focus asked for before the window was first placed.
    var focusWhenShown = false
    var ticks = 0
    /// Rex's palette or another of its windows is over the pane.
    var coveredByRex = false
    /// The last mouse or key event delivered to this window.
    var lastInputAt = Date.distantPast

    init(session: String, block: String) {
        self.session = session
        self.block = block
        window = PaneWindow(contentRect: NSRect(x: 0, y: 0, width: 640, height: 480),
                            styleMask: [.borderless], backing: .buffered, defer: false)
        super.init()
    }

    // MARK: Hooks

    /// The view that takes the keyboard when the pane is focused.
    var focusTarget: NSView? { nil }
    /// Shortcuts that stay with the pane even when Rex binds them.
    var reservedKeys: Set<Chord> { [] }
    /// Redraws in the current light or dark appearance.
    func applyAppearance() {}
    /// Last work before the helper exits.
    func willShutDown() {}

    /// Light or dark like Rex.
    func followRexAppearance() {
        let wanted = Chrome.appearance()
        guard NSApp.appearance?.name != wanted?.name else { return }
        NSApp.appearance = wanted
        applyAppearance()
    }

    @objc func closePane() {
        let session = session, block = block
        window.orderOut(nil)
        Rex.run(["block", "close", "--session", session, block])
        shutDown()
    }

    // MARK: Following the pane

    func start() {
        Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in self?.follow() }
            .tolerance = 0.002
        Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            if getppid() != self.parent { self.shutDown() }
            self.chrome = Chrome.current()
            self.followRexAppearance()
            self.refresh()
        }
        let center = NSWorkspace.shared.notificationCenter
        center.addObserver(forName: NSWorkspace.didActivateApplicationNotification, object: nil, queue: .main) {
            [weak self] note in self?.applicationActivated(note)
        }
        followRexAppearance()
        watchEvents()
        refresh()
        loadBindings()
    }

    func watchEvents() {
        let task = Process()
        if Rex.program.contains("/") {
            task.executableURL = URL(fileURLWithPath: Rex.program)
            task.arguments = ["events", "--session", session, "--json"]
        } else {
            task.executableURL = URL(fileURLWithPath: "/usr/bin/env")
            task.arguments = [Rex.program, "events", "--session", session, "--json"]
        }
        let pipe = Pipe()
        task.standardOutput = pipe
        task.standardError = FileHandle.nullDevice
        var pending = Data()
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            if chunk.isEmpty { return }
            pending.append(chunk)
            var relevant = false
            while let end = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let line = pending[pending.startIndex..<end]
                pending.removeSubrange(pending.startIndex...end)
                if PaneController.changesLayout(line) { relevant = true }
            }
            if relevant { DispatchQueue.main.async { self?.refresh() } }
        }
        task.terminationHandler = { [weak self] _ in
            pipe.fileHandleForReading.readabilityHandler = nil
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { self?.watchEvents() }
        }
        do { try task.run(); events = task } catch {
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in self?.watchEvents() }
        }
    }

    /// Every `rex` command, this helper's own included, connects as a client;
    /// refreshing on those events would refresh forever.
    static func changesLayout(_ line: Data) -> Bool {
        guard let object = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any],
              let payload = object["payload"] as? [String: Any],
              let event = payload["event"] as? [String: Any],
              let type = event["type"] as? String else { return true }
        return type != "client_connected" && type != "client_disconnected"
    }

    /// Fetches the session layout, one request at a time.
    func refresh() {
        if refreshing { refreshAgain = true; return }
        refreshing = true
        let session = session
        Rex.background {
            let view = Rex.call("session.view", ["session_id": session])
            DispatchQueue.main.async { self.apply(view) }
        }
    }

    func apply(_ view: [String: Any]?) {
        refreshing = false
        if let view {
            self.view = view
            if SessionView.contains(view, block: block) {
                missing = 0
            } else {
                missing += 1
                // Closed in Rex: nothing is left to show.
                if missing >= 2 { shutDown() }
            }
            let next = SessionView.placement(view, block: block)
            // Rex animates panes into place. Following that animation would
            // lag it, so step out of the way and come back once it settles.
            if next?.rect != placement?.rect {
                settleUntil = Date().addingTimeInterval(0.28)
                hide()
            }
            placement = next
            syncFocus()
        }
        if refreshAgain { refreshAgain = false; refresh() }
    }

    func pickRexWindow() -> RexWindow? {
        let windows = WindowServer.rexWindows(WindowServer.list())
        if let current = rex, let same = windows.first(where: { $0.number == current.number }) { return same }
        return windows.first
    }

    func follow() {
        ticks &+= 1
        // Rex's window only moves while Rex (or this pane) is in front;
        // otherwise a few checks a second are enough.
        let watching = NSApp.isActive || rexIsActive
        if !watching && ticks % 10 != 0 { return }
        guard let placement, Date() >= settleUntil else { hide(); return }
        var bounds = rex.flatMap { WindowServer.bounds(of: $0.number) }
        if bounds == nil {
            rex = pickRexWindow()
            bounds = rex.flatMap { WindowServer.bounds(of: $0.number) }
        }
        guard let target = rex, let bounds,
              let primary = NSScreen.screens.first?.frame.height else { hide(); return }
        let body = chrome.body(placement.rect, in: bounds)
        guard body.width > 80, body.height > 60 else { hide(); return }
        // Rex's palette and dialogs are windows of their own over the pane;
        // they must stay on top and keep the keyboard, so give way to them.
        if !watching || ticks % 3 == 0 {
            let covered = rexOverlay(covering: body, of: target)
            if !covered && coveredByRex && placement.focused && rexIsActive { focusWhenShown = true }
            coveredByRex = covered
        }
        if coveredByRex { hide(); return }
        let frame = NSRect(x: body.minX.rounded(), y: (primary - body.maxY).rounded(),
                           width: body.width.rounded(), height: body.height.rounded())
        if frame != lastFrame {
            window.setFrame(frame, display: true)
            lastFrame = frame
        }
        if root.layer?.cornerRadius != chrome.radius { root.layer?.cornerRadius = chrome.radius }
        if !shown {
            window.alphaValue = 0
            window.order(.above, relativeTo: Int(target.number))
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.12
                window.animator().alphaValue = 1
            }
        } else if ticks % 10 == 0 && !directlyAbove(target.number) {
            window.order(.above, relativeTo: Int(target.number))
        }
        shown = true
        if focusWhenShown {
            focusWhenShown = false
            takeFocus()
        }
    }

    /// Whether another window of Rex's, in front of its main window, overlaps
    /// `body` (screen points, top-left origin): the palette, a dialog, or a
    /// second Rex window.
    func rexOverlay(covering body: CGRect, of rex: RexWindow) -> Bool {
        for item in WindowServer.list() {
            guard let number = item[kCGWindowNumber as String] as? CGWindowID else { continue }
            // The list runs front to back: past Rex's window, nothing covers it.
            if number == rex.number { return false }
            guard item[kCGWindowOwnerPID as String] as? pid_t == rex.pid,
                  item[kCGWindowLayer as String] as? Int == 0,
                  let dictionary = item[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: dictionary) else { continue }
            if bounds.intersects(body) { return true }
        }
        return false
    }

    /// Whether only this helper's windows lie between this window and Rex's.
    func directlyAbove(_ rexNumber: CGWindowID) -> Bool {
        let mine = CGWindowID(window.windowNumber)
        let above = CGWindowListCopyWindowInfo([.optionOnScreenAboveWindow], rexNumber) as? [[String: Any]] ?? []
        let ownName = ProcessInfo.processInfo.processName
        // `above` runs front to back; walk back from the window right above Rex.
        for item in above.reversed() {
            guard let number = item[kCGWindowNumber as String] as? CGWindowID else { continue }
            if number == mine { return true }
            let layer = item[kCGWindowLayer as String] as? Int ?? 0
            if layer != 0 { continue }
            if item[kCGWindowOwnerName as String] as? String != ownName { return false }
        }
        return false
    }

    /// Stops the `rex events` watcher, then exits: every way out goes here.
    func shutDown() -> Never {
        willShutDown()
        events?.terminationHandler = nil
        events?.terminate()
        exit(0)
    }

    func hide() {
        guard shown else { return }
        window.orderOut(nil)
        shown = false
    }

    // MARK: Focus

    var rexIsActive: Bool {
        guard let pid = rex?.pid else { return false }
        return NSWorkspace.shared.frontmostApplication?.processIdentifier == pid
    }

    func syncFocus() {
        let focused = placement?.focused ?? false
        defer { wasFocusedInRex = focused }
        if focused && !wasFocusedInRex && rexIsActive {
            takeFocus()
        } else if !focused && wasFocusedInRex && NSApp.isActive {
            // Rex moved focus to another pane, by a forwarded key or its API.
            activateRex()
        }
    }

    func applicationActivated(_ note: Notification) {
        guard let app = note.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication,
              app.processIdentifier == rex?.pid else { return }
        // Rex raised its windows over this one; go back on top at once.
        if shown, let number = rex?.number { window.order(.above, relativeTo: Int(number)) }
        if placement?.focused == true && Date().timeIntervalSince(forwardedAt) > 1 { takeFocus() }
    }

    func takeFocus() {
        guard shown else { focusWhenShown = true; return }
        NSApp.activate(ignoringOtherApps: true)
        window.makeKey()
        if window.firstResponder == nil || window.firstResponder === window { window.makeFirstResponder(focusTarget) }
    }

    func activateRex() {
        guard let pid = rex?.pid else { return }
        NSRunningApplication(processIdentifier: pid)?.activate()
    }

    /// Clicking into this window makes its block Rex's focused one.
    func windowBecameKey() {
        guard placement?.focused == false else { return }
        let session = session, block = block
        Rex.background { _ = Rex.call("session.focus_block", ["session_id": session, "block_id": block]) }
    }

    // MARK: Rex shortcuts

    func loadBindings() {
        let session = session, reserved = reservedKeys
        Rex.background {
            var parsed: [Binding] = []
            if let data = Rex.run(["keymap", "--json"]),
               let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] {
                for item in object["bindings"] as? [[String: Any]] ?? [] {
                    guard let action = item["action"] as? String, action != "pane.send_key",
                          let keys = item["keys"] as? [String] else { continue }
                    let chords = keys.compactMap(Chord.init)
                    guard !chords.isEmpty, chords.count == keys.count,
                          !(chords.count == 1 && reserved.contains(chords[0])) else { continue }
                    parsed.append(Binding(keys: chords, action: action, args: item["args"]))
                }
            }
            var client: String?
            if let list = Rex.call("client.list", [:])?["clients"] as? [[String: Any]] {
                let apps = list.filter { ($0["info"] as? [String: Any])?["kind"] as? String == "app" }
                client = (apps.first { ($0["session_ids"] as? [String] ?? []).contains(session) } ?? apps.first)?["client_id"] as? String
            }
            DispatchQueue.main.async {
                self.bindings = parsed
                self.appClient = client
            }
        }
    }

    /// Handles a key that Rex binds; false lets the page and menus have it.
    func handleRexKey(_ event: NSEvent) -> Bool {
        guard let chord = Chord(event), !bindings.isEmpty else { return false }
        // No Rex app was connected when the bindings loaded, so nothing could
        // perform them: leave the key to the pane and look again.
        guard appClient != nil else {
            pendingPrefix = []
            loadBindings()
            return false
        }
        let typed = pendingPrefix + [chord]
        if let binding = bindings.first(where: { $0.keys == typed }) {
            pendingPrefix = []
            perform(binding)
            return true
        }
        if bindings.contains(where: { $0.keys.count > typed.count && Array($0.keys.prefix(typed.count)) == typed }) {
            pendingPrefix = typed
            return true
        }
        let swallowed = !pendingPrefix.isEmpty
        pendingPrefix = []
        return swallowed
    }

    func perform(_ binding: Binding) {
        guard let client = appClient else { return }
        forwardedAt = Date()
        var input: [String: Any] = ["client_id": client, "action": binding.action, "session_id": session,
                                    "block_id": block, "origin": "key"]
        if let args = binding.args, !(args is NSNull) { input["args"] = args }
        // Actions that open Rex UI need Rex in front to take the typing.
        let staysHere = binding.action.hasPrefix("pane.resize") || binding.action == "pane.zoom"
        if !staysHere { activateRex() }
        Rex.background { _ = Rex.call("client.perform", input) }
    }
}

// MARK: - App

enum PaneApp {
    /// The Rex session and block this helper's pane is, from the environment
    /// Rex sets in a pane; exits with a message naming `what` without them.
    static func rexBlock(for what: String) -> (session: String, block: String) {
        let environment = ProcessInfo.processInfo.environment
        guard let session = environment["REX_SESSION"], !session.isEmpty,
              let block = environment["REX_BLOCK"], !block.isEmpty else {
            FileHandle.standardError.write(Data("leaf: \(what) needs REX_SESSION and REX_BLOCK; open it from a Rex pane.\n".utf8))
            exit(1)
        }
        return (session, block)
    }

    /// Runs the app with `delegate`, which this call keeps alive: it never returns.
    static func run(_ delegate: NSApplicationDelegate) -> Never {
        signal(SIGPIPE, SIG_IGN)
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        app.delegate = delegate
        app.run()
        withExtendedLifetime(delegate) {}
        exit(0)
    }
}
