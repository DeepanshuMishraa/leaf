// Leaf's WebKit pane: a borderless window kept exactly over one Rex block.
//
// Rex draws its blocks itself and has no way to host a native view, so this
// helper owns a window and follows the block: the session layout (fractions of
// the window) comes from `rex api call session.view`, refreshed whenever the
// session emits an event; the Rex window's frame comes from the window server
// every frame; the space Rex's own chrome takes (sidebar, toolbar, gaps,
// headers) comes from Rex's preferences. The window sits directly above the
// Rex window at the normal level, so other apps still cover both.
//
// Environment: REX_SESSION, REX_BLOCK (set by Rex in the pane), LEAF_REX (the
// rex program), LEAF_BROWSER_URL (optional first page).

import AppKit
import WebKit

// MARK: - Rex

enum Rex {
    static let program = ProcessInfo.processInfo.environment["LEAF_REX"] ?? "rex"
    static let queue = DispatchQueue(label: "leaf.browser.rex", attributes: .concurrent)

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
            }
        }
        return false
    }

    /// The block's place when it's drawn in a tiled layout of the active
    /// window; nil when another tab is showing, another block is zoomed, or
    /// the block floats (Leaf opens browsers as splits).
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

/// The address pill: a lock or globe, then the field.
final class AddressField: NSTextField {
    var onFocus: (() -> Void)?

    override func becomeFirstResponder() -> Bool {
        let accepted = super.becomeFirstResponder()
        if accepted { onFocus?() }
        return accepted
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

// MARK: - Browser

final class Browser: NSObject, WKNavigationDelegate, WKUIDelegate, WKDownloadDelegate, NSSearchFieldDelegate {
    let session: String
    let block: String
    let window: PaneWindow
    let root = NSView()
    let bar = Bar()
    let web: WKWebView
    let back: ToolButton
    let forward: ToolButton
    let reload: ToolButton
    let pill = NSView()
    let badge = NSImageView()
    let address = AddressField()
    let progress = CALayer()
    let findBar = Bar()
    let findField = NSSearchField()
    var findShown = false

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

    // Page
    var failedURL: URL?
    var errorNavigation: WKNavigation?
    var titleSent = ""
    var observers: [NSKeyValueObservation] = []
    var downloads: [WKDownload: URL] = [:]

    static let barHeight: CGFloat = 40

    init(session: String, block: String) {
        self.session = session
        self.block = block
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .default()
        configuration.applicationNameForUserAgent = Browser.userAgentSuffix()
        configuration.preferences.isElementFullscreenEnabled = true
        web = WKWebView(frame: .zero, configuration: configuration)
        window = PaneWindow(contentRect: NSRect(x: 0, y: 0, width: 640, height: 480),
                            styleMask: [.borderless], backing: .buffered, defer: false)
        back = ToolButton(symbol: "chevron.left", label: "Back", target: nil, action: #selector(Browser.goBack))
        forward = ToolButton(symbol: "chevron.right", label: "Forward", target: nil, action: #selector(Browser.goForward))
        reload = ToolButton(symbol: "arrow.clockwise", label: "Reload", target: nil, action: #selector(Browser.reloadOrStop))
        super.init()
        for button in [back, forward, reload] { button.target = self }
        build()
    }

    /// Sites sniff for "Safari" and serve worse pages without it.
    static func userAgentSuffix() -> String {
        let info = NSDictionary(contentsOfFile: "/Applications/Safari.app/Contents/Info.plist")
        let version = info?["CFBundleShortVersionString"] as? String ?? "18.0"
        return "Version/\(version) Safari/605.1.15"
    }

    // MARK: Building

    func build() {
        window.isReleasedWhenClosed = false
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = false
        window.level = .normal
        window.collectionBehavior = [.managed, .fullScreenAuxiliary, .ignoresCycle]
        window.title = "Leaf Browser"
        window.animationBehavior = .none

        root.wantsLayer = true
        root.layer?.masksToBounds = true
        root.layer?.cornerCurve = .continuous
        window.contentView = root

        root.addSubview(bar)
        for button in [back, forward, reload] { bar.addSubview(button) }

        pill.wantsLayer = true
        pill.layer?.cornerRadius = 8
        pill.layer?.cornerCurve = .continuous
        bar.addSubview(pill)
        badge.imageScaling = .scaleProportionallyDown
        badge.contentTintColor = .tertiaryLabelColor
        pill.addSubview(badge)

        address.isBordered = false
        address.drawsBackground = false
        address.focusRingType = .none
        address.font = .systemFont(ofSize: 12.5)
        address.textColor = .labelColor
        address.lineBreakMode = .byTruncatingTail
        address.usesSingleLineMode = true
        address.cell?.isScrollable = true
        address.placeholderString = "Search or enter address"
        address.delegate = self
        address.target = self
        address.action = #selector(submitAddress)
        address.onFocus = { [weak self] in self?.addressFocused() }
        pill.addSubview(address)

        progress.backgroundColor = NSColor.controlAccentColor.cgColor
        progress.opacity = 0
        bar.wantsLayer = true
        bar.layer?.addSublayer(progress)

        findBar.isHidden = true
        findField.placeholderString = "Find on page"
        findField.font = .systemFont(ofSize: 12)
        findField.focusRingType = .none
        findField.sendsSearchStringImmediately = true
        findField.target = self
        findField.action = #selector(findChanged)
        findField.delegate = self
        findBar.addSubview(findField)
        let done = ToolButton(symbol: "xmark", label: "Close Find", target: self, action: #selector(closeFind))
        done.identifier = NSUserInterfaceItemIdentifier("done")
        findBar.addSubview(done)
        root.addSubview(findBar)

        web.navigationDelegate = self
        web.uiDelegate = self
        web.allowsBackForwardNavigationGestures = true
        web.allowsMagnification = true
        if #available(macOS 13.3, *) { web.isInspectable = true }
        root.addSubview(web)
        window.initialFirstResponder = web

        observers = [
            web.observe(\.estimatedProgress, options: [.new]) { [weak self] _, _ in self?.updateProgress() },
            web.observe(\.isLoading, options: [.new]) { [weak self] _, _ in self?.updateControls() },
            web.observe(\.canGoBack, options: [.new]) { [weak self] _, _ in self?.updateControls() },
            web.observe(\.canGoForward, options: [.new]) { [weak self] _, _ in self?.updateControls() },
            web.observe(\.url, options: [.new]) { [weak self] _, _ in self?.showURL() },
            web.observe(\.title, options: [.new]) { [weak self] _, _ in self?.sendTitle() },
        ]

        NotificationCenter.default.addObserver(forName: NSView.frameDidChangeNotification, object: root, queue: .main) {
            [weak self] _ in self?.layout()
        }
        root.postsFrameChangedNotifications = true
        applyAppearance()
        DistributedNotificationCenter.default().addObserver(forName: NSNotification.Name("AppleInterfaceThemeChangedNotification"),
                                                            object: nil, queue: .main) { [weak self] _ in
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { self?.applyAppearance() }
        }
        updateControls()
        layout()
    }

    func applyAppearance() {
        root.effectiveAppearance.performAsCurrentDrawingAppearance {
            root.layer?.backgroundColor = NSColor.textBackgroundColor.cgColor
            pill.layer?.backgroundColor = NSColor.labelColor.withAlphaComponent(0.06).cgColor
            progress.backgroundColor = NSColor.controlAccentColor.cgColor
        }
        bar.needsDisplay = true
        findBar.needsDisplay = true
    }

    func layout() {
        let width = root.bounds.width, height = root.bounds.height
        let barHeight = Browser.barHeight
        bar.frame = NSRect(x: 0, y: height - barHeight, width: width, height: barHeight)
        // `bar` is flipped: y grows downward inside it.
        let size: CGFloat = 28, top = (barHeight - size) / 2
        back.frame = NSRect(x: 8, y: top, width: size, height: size)
        forward.frame = NSRect(x: 8 + size + 2, y: top, width: size, height: size)
        reload.frame = NSRect(x: 8 + 2 * (size + 2), y: top, width: size, height: size)
        let pillX = reload.frame.maxX + 8
        pill.frame = NSRect(x: pillX, y: top, width: max(60, width - pillX - 10), height: size)
        badge.frame = NSRect(x: 9, y: (size - 14) / 2, width: 14, height: 14)
        let fieldHeight: CGFloat = 17
        address.frame = NSRect(x: 29, y: (size - fieldHeight) / 2, width: pill.frame.width - 37, height: fieldHeight)
        layoutProgress(animated: false)

        let findHeight: CGFloat = findShown ? 36 : 0
        findBar.frame = NSRect(x: 0, y: height - barHeight - findHeight, width: width, height: findHeight)
        findField.frame = NSRect(x: 10, y: 6, width: min(320, width - 56), height: 24)
        if let done = findBar.subviews.first(where: { $0.identifier?.rawValue == "done" }) {
            done.frame = NSRect(x: findField.frame.maxX + 6, y: 4, width: 28, height: 28)
        }
        web.frame = NSRect(x: 0, y: 0, width: width, height: max(0, height - barHeight - findHeight))
    }

    // MARK: Page state

    func updateControls() {
        back.isEnabled = web.canGoBack
        forward.isEnabled = web.canGoForward
        if web.isLoading {
            reload.setSymbol("xmark", label: "Stop")
        } else {
            reload.setSymbol("arrow.clockwise", label: "Reload")
        }
        updateProgress()
        showURL()
    }

    func updateProgress() {
        if web.isLoading {
            progress.opacity = 1
            layoutProgress(animated: true)
        } else {
            CATransaction.begin()
            CATransaction.setAnimationDuration(0.25)
            progress.frame.size.width = bar.bounds.width
            progress.opacity = 0
            CATransaction.commit()
        }
    }

    func layoutProgress(animated: Bool) {
        CATransaction.begin()
        CATransaction.setDisableActions(!animated)
        CATransaction.setAnimationDuration(0.2)
        let fraction = web.isLoading ? max(0.08, web.estimatedProgress) : (progress.opacity > 0 ? 1 : 0)
        progress.frame = CGRect(x: 0, y: bar.bounds.height - 2, width: bar.bounds.width * fraction, height: 2)
        CATransaction.commit()
    }

    var editingAddress: Bool { address.currentEditor() != nil }

    func showURL() {
        let url = failedURL ?? web.url
        let secure = url?.scheme == "https"
        let symbol = url == nil ? "magnifyingglass" : (secure ? "lock.fill" : "globe")
        badge.image = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 10, weight: .semibold))
        guard !editingAddress else { return }
        address.stringValue = url.map(Browser.display) ?? ""
    }

    /// A URL as people read it: no scheme or "www.", no lone trailing slash.
    static func display(_ url: URL) -> String {
        guard let scheme = url.scheme, scheme == "http" || scheme == "https" else { return url.absoluteString }
        var text = url.absoluteString
        text.removeFirst(scheme.count + 3)
        if text.hasPrefix("www.") { text.removeFirst(4) }
        if text.hasSuffix("/") && text.filter({ $0 == "/" }).count == 1 { text.removeLast() }
        return text
    }

    func addressFocused() {
        if let url = failedURL ?? web.url { address.stringValue = url.absoluteString }
        DispatchQueue.main.async { self.address.currentEditor()?.selectAll(nil) }
    }

    func controlTextDidEndEditing(_ notification: Notification) {
        // The field editor is still attached during this call.
        if notification.object as? NSTextField === address { DispatchQueue.main.async { self.showURL() } }
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        if control === address, selector == #selector(NSResponder.cancelOperation(_:)) {
            window.makeFirstResponder(web)
            DispatchQueue.main.async { self.showURL() }
            return true
        }
        if control === findField {
            if selector == #selector(NSResponder.cancelOperation(_:)) { closeFind(); return true }
            if selector == #selector(NSResponder.insertNewline(_:)) {
                let backwards = NSApp.currentEvent?.modifierFlags.contains(.shift) ?? false
                find(backwards: backwards)
                return true
            }
        }
        return false
    }

    func sendTitle() {
        let title = (web.title ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let label = title.isEmpty ? (web.url?.host ?? "browser") : String(title.prefix(48))
        guard label != titleSent else { return }
        titleSent = label
        let session = session, block = block
        Rex.background { Rex.run(["block", "rename", "-s", session, block, label]) }
    }

    // MARK: Navigation

    static func url(from text: String) -> URL? {
        let input = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !input.isEmpty else { return nil }
        if input.hasPrefix("/") || input.hasPrefix("~/") {
            return URL(fileURLWithPath: (input as NSString).expandingTildeInPath)
        }
        if let url = URL(string: input), let scheme = url.scheme?.lowercased(),
           ["http", "https", "file", "about", "data"].contains(scheme) {
            return url
        }
        let local = input.range(of: #"^(localhost|127(\.\d{1,3}){3}|\[[0-9a-fA-F:]+\]|(\d{1,3}\.){3}\d{1,3}|[\w-]+)(:\d+)(/.*)?$"#,
                                options: .regularExpression) != nil
            || input == "localhost" || input.hasPrefix("localhost/")
        if local { return URL(string: "http://" + input) }
        let looksLikeHost = !input.contains(" ")
            && input.range(of: #"^[^\s/]+\.[A-Za-z]{2,}(:\d+)?(/.*)?$"#, options: .regularExpression) != nil
        if looksLikeHost { return URL(string: "https://" + input) }
        var query = CharacterSet.urlQueryAllowed
        query.remove(charactersIn: "&+=?#")
        let encoded = input.addingPercentEncoding(withAllowedCharacters: query) ?? input
        return URL(string: "https://duckduckgo.com/?q=" + encoded)
    }

    func navigate(_ text: String) {
        guard let url = Browser.url(from: text) else { return }
        load(url)
    }

    func load(_ url: URL) {
        failedURL = nil
        if url.isFileURL {
            web.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
        } else {
            web.load(URLRequest(url: url))
        }
    }

    @objc func submitAddress() {
        let text = address.stringValue
        guard !text.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        window.makeFirstResponder(web)
        navigate(text)
    }

    @objc func goBack() { web.goBack() }
    @objc func goForward() { web.goForward() }
    @objc func reloadOrStop() {
        if web.isLoading { web.stopLoading(); return }
        if let failed = failedURL { load(failed) } else { web.reload() }
    }
    @objc func reloadPage() {
        if let failed = failedURL { load(failed) } else { web.reload() }
    }
    @objc func hardReload() { web.reloadFromOrigin() }
    @objc func focusAddress() {
        window.makeFirstResponder(address)
    }
    @objc func zoomIn() { web.pageZoom = min(3, web.pageZoom + 0.1) }
    @objc func zoomOut() { web.pageZoom = max(0.3, web.pageZoom - 0.1) }
    @objc func zoomReset() { web.pageZoom = 1 }

    @objc func closePane() {
        let session = session, block = block
        window.orderOut(nil)
        Rex.run(["block", "close", "--session", session, block])
        exit(0)
    }

    // MARK: Find

    @objc func showFind() {
        if !findShown {
            findShown = true
            findBar.isHidden = false
            layout()
        }
        window.makeFirstResponder(findField)
        findField.currentEditor()?.selectAll(nil)
    }

    @objc func closeFind() {
        guard findShown else { return }
        findShown = false
        findBar.isHidden = true
        layout()
        window.makeFirstResponder(web)
    }

    @objc func findChanged() { find(backwards: false, fromStart: true) }
    @objc func findNext() { if findShown { find(backwards: false) } else { showFind() } }
    @objc func findPrevious() { if findShown { find(backwards: true) } else { showFind() } }

    func find(backwards: Bool, fromStart: Bool = false) {
        let text = findField.stringValue
        guard !text.isEmpty else { return }
        let configuration = WKFindConfiguration()
        configuration.backwards = backwards
        configuration.wraps = true
        configuration.caseSensitive = false
        web.find(text, configuration: configuration) { [weak self] result in
            self?.findField.textColor = result.matchFound ? .labelColor : .systemRed
        }
    }

    // MARK: WKNavigationDelegate

    func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction,
                 decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
        guard let url = navigationAction.request.url, let scheme = url.scheme?.lowercased() else {
            decisionHandler(.allow)
            return
        }
        // mailto:, zoommtg:, App Store links and the like belong to their apps.
        if !["http", "https", "file", "about", "data", "blob", "javascript"].contains(scheme) {
            NSWorkspace.shared.open(url)
            decisionHandler(.cancel)
            return
        }
        if navigationAction.shouldPerformDownload {
            decisionHandler(.download)
            return
        }
        decisionHandler(.allow)
    }

    func webView(_ webView: WKWebView, decidePolicyFor navigationResponse: WKNavigationResponse,
                 decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        let attachment = (navigationResponse.response as? HTTPURLResponse)?
            .value(forHTTPHeaderField: "Content-Disposition")?.lowercased().hasPrefix("attachment") ?? false
        decisionHandler(navigationResponse.canShowMIMEType && !attachment ? .allow : .download)
    }

    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) {
        download.delegate = self
    }

    func download(_ download: WKDownload, decideDestinationUsing response: URLResponse, suggestedFilename: String,
                  completionHandler: @escaping (URL?) -> Void) {
        let folder = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first
            ?? FileManager.default.homeDirectoryForCurrentUser
        let name = (suggestedFilename as NSString).lastPathComponent
        let stem = (name as NSString).deletingPathExtension, ext = (name as NSString).pathExtension
        var target = folder.appendingPathComponent(name)
        var index = 2
        while FileManager.default.fileExists(atPath: target.path) {
            target = folder.appendingPathComponent(ext.isEmpty ? "\(stem) \(index)" : "\(stem) \(index).\(ext)")
            index += 1
        }
        downloads[download] = target
        completionHandler(target)
    }

    func downloadDidFinish(_ download: WKDownload) {
        if let target = downloads.removeValue(forKey: download) {
            NSWorkspace.shared.activateFileViewerSelecting([target])
        }
    }

    func download(_ download: WKDownload, didFailWithError error: Error, resumeData: Data?) {
        let name = downloads.removeValue(forKey: download)?.lastPathComponent ?? "the file"
        notice("Couldn't download \(name)", (error as NSError).localizedDescription)
    }

    func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
        // The error page keeps showing the address that failed.
        if navigation !== errorNavigation { failedURL = nil }
        if findShown { closeFind() }
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        showURL()
        sendTitle()
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        failed(error)
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        failed(error)
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
        web.reload()
    }

    func failed(_ error: Error) {
        let error = error as NSError
        // Cancelled by a newer navigation, or a download taking over.
        if error.domain == NSURLErrorDomain && error.code == NSURLErrorCancelled { return }
        if error.domain == "WebKitErrorDomain" && (error.code == 102 || error.code == 204) { return }
        let url = (error.userInfo[NSURLErrorFailingURLErrorKey] as? URL) ?? web.url
        failedURL = url
        let host = url?.host ?? url?.absoluteString ?? "This page"
        let reason: String
        switch error.code {
        case NSURLErrorCannotFindHost, NSURLErrorDNSLookupFailed:
            reason = "\(host) couldn't be found. Check the address for typos."
        case NSURLErrorCannotConnectToHost:
            reason = "\(host) refused the connection. If it's a local server, make sure it's running."
        case NSURLErrorNotConnectedToInternet, NSURLErrorNetworkConnectionLost:
            reason = "This Mac isn't connected to the internet."
        case NSURLErrorTimedOut:
            reason = "\(host) took too long to answer."
        case NSURLErrorServerCertificateUntrusted, NSURLErrorServerCertificateHasBadDate,
             NSURLErrorServerCertificateHasUnknownRoot, NSURLErrorServerCertificateNotYetValid,
             NSURLErrorSecureConnectionFailed:
            reason = "The connection to \(host) isn't secure, so it wasn't opened."
        default:
            reason = error.localizedDescription
        }
        errorNavigation = web.loadHTMLString(Browser.errorPage(title: "Can't open this page", reason: reason),
                                             baseURL: nil)
        showURL()
    }

    static func errorPage(title: String, reason: String) -> String {
        func escape(_ text: String) -> String {
            text.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
                .replacingOccurrences(of: ">", with: "&gt;")
        }
        return """
        <!doctype html><meta charset="utf-8"><meta name="color-scheme" content="light dark">
        <style>
        html,body{height:100%;margin:0}
        body{display:grid;place-items:center;font:14px/1.5 -apple-system,system-ui;color:#6b7280;background:Canvas}
        main{max-width:420px;padding:24px}
        h1{font-size:17px;font-weight:600;color:CanvasText;margin:0 0 6px}
        p{margin:0}
        kbd{font:12px ui-monospace,monospace;padding:1px 5px;border-radius:4px;background:rgba(127,127,127,.15)}
        </style>
        <main><h1>\(escape(title))</h1><p>\(escape(reason))</p><p style="margin-top:14px">Press <kbd>⌘R</kbd> to try again.</p></main>
        """
    }

    // MARK: WKUIDelegate

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        // One page per pane: links meant for a new window open here.
        if navigationAction.targetFrame == nil, let url = navigationAction.request.url {
            load(url)
        }
        return nil
    }

    func webViewDidClose(_ webView: WKWebView) {}

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        let alert = NSAlert()
        alert.messageText = frame.request.url?.host ?? "This page says"
        alert.informativeText = message
        alert.addButton(withTitle: "OK")
        alert.beginSheetModal(for: window) { _ in completionHandler() }
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        let alert = NSAlert()
        alert.messageText = frame.request.url?.host ?? "This page asks"
        alert.informativeText = message
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        alert.beginSheetModal(for: window) { completionHandler($0 == .alertFirstButtonReturn) }
    }

    func webView(_ webView: WKWebView, runJavaScriptTextInputPanelWithPrompt prompt: String, defaultText: String?,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (String?) -> Void) {
        let alert = NSAlert()
        alert.messageText = frame.request.url?.host ?? "This page asks"
        alert.informativeText = prompt
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 22))
        field.stringValue = defaultText ?? ""
        alert.accessoryView = field
        alert.addButton(withTitle: "OK")
        alert.addButton(withTitle: "Cancel")
        alert.beginSheetModal(for: window) { completionHandler($0 == .alertFirstButtonReturn ? field.stringValue : nil) }
    }

    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.canChooseDirectories = parameters.allowsDirectories
        panel.canChooseFiles = true
        panel.beginSheetModal(for: window) { completionHandler($0 == .OK ? panel.urls : nil) }
    }

    func notice(_ title: String, _ detail: String) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = detail
        alert.beginSheetModal(for: window)
    }

    // MARK: Following the pane

    func start() {
        Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in self?.follow() }
            .tolerance = 0.002
        Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            if getppid() != self.parent { exit(0) }
            self.chrome = Chrome.current()
            self.refresh()
        }
        let center = NSWorkspace.shared.notificationCenter
        center.addObserver(forName: NSWorkspace.didActivateApplicationNotification, object: nil, queue: .main) {
            [weak self] note in self?.applicationActivated(note)
        }
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
                if Browser.changesLayout(line) { relevant = true }
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
                if missing >= 2 { events?.terminate(); exit(0) }
            }
            let next = SessionView.placement(view, block: block)
            if next?.rect != placement?.rect { settleUntil = Date().addingTimeInterval(0.12) }
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
        let frame = NSRect(x: body.minX.rounded(), y: (primary - body.maxY).rounded(),
                           width: body.width.rounded(), height: body.height.rounded())
        if frame != lastFrame {
            window.setFrame(frame, display: true)
            lastFrame = frame
        }
        if root.layer?.cornerRadius != chrome.radius { root.layer?.cornerRadius = chrome.radius }
        if !shown || (ticks % 10 == 0 && !directlyAbove(target.number)) {
            window.order(.above, relativeTo: Int(target.number))
        }
        shown = true
        if focusWhenShown {
            focusWhenShown = false
            takeFocus()
        }
    }

    /// Whether only Leaf browser windows lie between this window and Rex's.
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
        if window.firstResponder == nil || window.firstResponder === window { window.makeFirstResponder(web) }
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

    /// Browser shortcuts that stay with the page even when Rex binds them.
    static let reserved: Set<Chord> = Set([
        "cmd+c", "cmd+v", "cmd+x", "cmd+a", "cmd+z", "shift+cmd+z", "cmd+l", "cmd+r", "shift+cmd+r", "cmd+w",
        "cmd+f", "cmd+g", "shift+cmd+g", "cmd+[", "cmd+]", "cmd+=", "cmd+-", "cmd+0", "cmd+q", "cmd+.",
    ].compactMap(Chord.init))

    func loadBindings() {
        let session = session
        Rex.background {
            var parsed: [Binding] = []
            if let data = Rex.run(["keymap", "--json"]),
               let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] {
                for item in object["bindings"] as? [[String: Any]] ?? [] {
                    guard let action = item["action"] as? String, action != "pane.send_key",
                          let keys = item["keys"] as? [String] else { continue }
                    let chords = keys.compactMap(Chord.init)
                    guard !chords.isEmpty, chords.count == keys.count,
                          !(chords.count == 1 && Browser.reserved.contains(chords[0])) else { continue }
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

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    let browser: Browser

    init(browser: Browser) {
        self.browser = browser
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = menu()
        browser.window.delegate = self
        NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            return self.browser.handleRexKey(event) ? nil : event
        }
        browser.start()
        let initial = ProcessInfo.processInfo.environment["LEAF_BROWSER_URL"] ?? ""
        // Wait for the first layout so the window appears in place, focused.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
            self.browser.takeFocus()
            if initial.isEmpty {
                self.browser.focusAddress()
            } else {
                self.browser.window.makeFirstResponder(self.browser.web)
                self.browser.navigate(initial)
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func windowDidBecomeKey(_ notification: Notification) { browser.windowBecameKey() }

    func menu() -> NSMenu {
        let main = NSMenu()
        func submenu(_ title: String, _ items: [NSMenuItem]) {
            let holder = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            let menu = NSMenu(title: title)
            items.forEach(menu.addItem)
            holder.submenu = menu
            main.addItem(holder)
        }
        func item(_ title: String, _ action: Selector, _ key: String, _ modifiers: NSEvent.ModifierFlags = .command,
                  target: AnyObject? = nil) -> NSMenuItem {
            let entry = NSMenuItem(title: title, action: action, keyEquivalent: key)
            entry.keyEquivalentModifierMask = modifiers
            entry.target = target
            return entry
        }
        let b = browser
        submenu("Leaf", [
            item("Close Pane", #selector(Browser.closePane), "w", target: b),
            item("Quit", #selector(Browser.closePane), "q", target: b),
        ])
        submenu("Edit", [
            item("Undo", Selector(("undo:")), "z"),
            item("Redo", Selector(("redo:")), "z", [.command, .shift]),
            .separator(),
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(NSText.paste(_:)), "v"),
            item("Paste and Match Style", #selector(NSTextView.pasteAsPlainText(_:)), "v", [.command, .option, .shift]),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
            .separator(),
            item("Find…", #selector(Browser.showFind), "f", target: b),
            item("Find Next", #selector(Browser.findNext), "g", target: b),
            item("Find Previous", #selector(Browser.findPrevious), "g", [.command, .shift], target: b),
        ])
        submenu("View", [
            item("Reload", #selector(Browser.reloadPage), "r", target: b),
            item("Reload Ignoring Cache", #selector(Browser.hardReload), "r", [.command, .shift], target: b),
            item("Stop", #selector(WKWebView.stopLoading(_:)), "."),
            .separator(),
            item("Actual Size", #selector(Browser.zoomReset), "0", target: b),
            item("Zoom In", #selector(Browser.zoomIn), "=", target: b),
            item("Zoom In", #selector(Browser.zoomIn), "+", target: b),
            item("Zoom Out", #selector(Browser.zoomOut), "-", target: b),
        ])
        submenu("Go", [
            item("Back", #selector(Browser.goBack), "[", target: b),
            item("Forward", #selector(Browser.goForward), "]", target: b),
            item("Open Location…", #selector(Browser.focusAddress), "l", target: b),
        ])
        return main
    }
}

let environment = ProcessInfo.processInfo.environment
guard let session = environment["REX_SESSION"], !session.isEmpty,
      let block = environment["REX_BLOCK"], !block.isEmpty else {
    FileHandle.standardError.write(Data("leaf: the browser needs REX_SESSION and REX_BLOCK; open it from a Rex pane.\n".utf8))
    exit(1)
}
signal(SIGPIPE, SIG_IGN)
let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let browser = Browser(session: session, block: block)
let delegate = AppDelegate(browser: browser)
app.delegate = delegate
app.run()
