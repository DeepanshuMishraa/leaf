// Leaf's WebKit pane: a borderless window kept exactly over one Rex block.
//
// Rex draws its blocks itself and has no way to host a native view, so this
// helper owns a window and follows the block: the session layout (fractions of
// the window) comes from `rex api call session.view`, refreshed whenever the
// session emits an event; the Rex window's frame comes from the window server
// every frame; the space Rex's own chrome takes (sidebar, toolbar, gaps,
// headers) comes from Rex's preferences. The page floats while Rex or a Leaf
// helper is frontmost; otherwise it sits just above Rex at the normal level.
//
// Environment: REX_SESSION (the session ID, which leaf browser resolves when
// Rex sets only REX_BLOCK), REX_BLOCK (set by Rex in the pane), LEAF_REX (the
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
    let floating: Bool
    /// The correction for the gaps introduced by each split above the block.
    let gapOffset: CGRect
    /// The outer floating layer, in the same normalized coordinates as `rect`.
    let layerBounds: CGRect?
    var layerID: String? = nil
    var upperLayers: [CGRect] = []

    /// The floating layer moved or changed size, but the block kept its slot within it.
    /// All helpers see this in session.view, even after the last resize command has completed.
    func followsLayer(from old: Placement?) -> Bool {
        guard let old, floating, old.floating, let layerID, old.layerID == layerID,
              let a = old.layerBounds, let b = layerBounds,
              a.width > 0, a.height > 0, b.width > 0, b.height > 0 else { return false }
        func same(_ x: CGFloat, _ y: CGFloat) -> Bool { abs(x - y) < 0.000001 }
        return same((old.rect.minX - a.minX) / a.width, (rect.minX - b.minX) / b.width) &&
               same((old.rect.minY - a.minY) / a.height, (rect.minY - b.minY) / b.height) &&
               same(old.rect.width / a.width, rect.width / b.width) &&
               same(old.rect.height / a.height, rect.height / b.height) &&
               same(old.gapOffset.origin.x, gapOffset.origin.x) && same(old.gapOffset.origin.y, gapOffset.origin.y) &&
               same(old.gapOffset.size.width, gapOffset.size.width) && same(old.gapOffset.size.height, gapOffset.size.height)
    }
}

enum SessionView {
    struct BlockPlacement {
        let rect: CGRect
        let gapOffset: CGRect
    }

    static func rect(_ value: Any?) -> CGRect? {
        guard let numbers = value as? [NSNumber], numbers.count == 4 else { return nil }
        return CGRect(x: numbers[0].doubleValue, y: numbers[1].doubleValue,
                      width: numbers[2].doubleValue, height: numbers[3].doubleValue)
    }

    /// Finds a block in a layout tree and carries the split-gap correction back
    /// up to the layer's coordinate system. Rex's rects already include the
    /// layer's position, so a floating layer's bounds must not be applied again.
    static func find(_ block: String, in node: Any?) -> BlockPlacement? {
        if let object = node as? [String: Any] {
            if object["block_id"] as? String == block, let rect = rect(object["rect"]) {
                return BlockPlacement(rect: rect, gapOffset: .zero)
            }
            guard let split = object["split"] as? [String: Any] else { return nil }
            let parent = rect(object["rect"])
            for (branch, child) in [("before", split["before"]), ("after", split["after"])] {
                guard let found = find(block, in: child) else { continue }
                var gapOffset = found.gapOffset
                let after = branch == "after" ? 1.0 : 0.0
                if let parent, parent.width > 0, parent.height > 0 {
                    if split["direction"] as? String == "vertical" {
                        gapOffset.origin.y += after - (found.rect.minY - parent.minY) / parent.height
                        gapOffset.size.height -= found.rect.height / parent.height
                    } else {
                        gapOffset.origin.x += after - (found.rect.minX - parent.minX) / parent.width
                        gapOffset.size.width -= found.rect.width / parent.width
                    }
                }
                return BlockPlacement(rect: found.rect, gapOffset: gapOffset)
            }
        } else if let array = node as? [Any] {
            for child in array {
                if let found = find(block, in: child) { return found }
            }
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
                // well so a view without `blocks` never closes the browser.
                if find(block, in: layer["layout"]) != nil { return true }
            }
        }
        return false
    }

    /// The block's place in the active window. Tiled and floating layers share
    /// the same normalized coordinate system in `session.view`.
    static func placement(_ view: [String: Any], block: String) -> Placement? {
        guard let active = view["active_window_id"] as? String,
              let window = (view["windows"] as? [[String: Any]])?.first(where: { $0["window_id"] as? String == active })
        else { return nil }
        let focused = window["focused_block_id"] as? String == block
        if let zoomed = window["zoomed_block_id"] as? String, !zoomed.isEmpty {
            let full = CGRect(x: 0, y: 0, width: 1, height: 1)
            return zoomed == block ? Placement(rect: full, focused: focused, floating: false, gapOffset: .zero, layerBounds: nil) : nil
        }
        let layers = window["layers"] as? [[String: Any]] ?? []
        for (index, layer) in layers.enumerated() {
            guard let kind = layer["kind"] as? String, kind == "tiled" || kind == "floating",
                  layer["state"] as? String != "minimized",
                  let found = find(block, in: layer["layout"]) else { continue }
            // Higher floating layers may cover the browser. Resolve their bounds in
            // screen points because the tiled and floating canvases can start at different heights.
            var upperLayers = [CGRect]()
            for upper in layers.dropFirst(index + 1) {
                guard upper["state"] as? String != "minimized", let layout = upper["layout"], !(layout is NSNull) else { continue }
                guard let bounds = rect(upper["bounds"]) else { return nil }
                upperLayers.append(bounds)
            }
            return Placement(rect: found.rect, focused: focused, floating: kind == "floating",
                             gapOffset: found.gapOffset, layerBounds: rect(layer["bounds"]),
                             layerID: layer["layer_id"] as? String, upperLayers: upperLayers)
        }
        return nil
    }
}

/// The space Rex's window chrome takes around and between tiled panes, read
/// from Rex's own preferences so it tracks the person's settings.
struct Chrome: Equatable {
    var top: CGFloat, left: CGFloat, right: CGFloat, bottom: CGFloat
    var gap: CGFloat, header: CGFloat, radius: CGFloat
    /// The top of Rex's floating canvas. It can include a title area that the
    /// tiled pane area does not have when tabs are vertical.
    var floatTop: CGFloat

    static let cardRadius: CGFloat = 10
    static let minimumBody = CGSize(width: 81, height: 61)
    static let domain = "com.superlogical.rex" as CFString

    static func setting(_ key: String) -> Any? {
        CFPreferencesCopyAppValue("Workspace.\(key)" as CFString, domain)
    }

    static func sessionID(from value: Any?) -> String? {
        guard let data = value as? Data ?? (value as? String)?.data(using: .utf8),
              let info = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let id = info["sessionID"] as? String, !id.isEmpty else { return nil }
        return id
    }

    static func shownSession() -> String? {
        CFPreferencesAppSynchronize(domain)
        return sessionID(from: setting("lastFocusedSession"))
    }

    static func current() -> Chrome {
        CFPreferencesAppSynchronize(domain)
        let compact = setting("paneDensity") as? String == "compact"
        let edge: CGFloat = compact ? 0 : 8
        // Measured from Rex 0.1.0-preview.23. Compact panes are split by a
        // 1pt rule under a 34pt header.
        var chrome = Chrome(top: compact ? 41 : 46, left: edge, right: edge, bottom: edge,
                            gap: compact ? 1 : 8, header: compact ? 34 : 36, radius: compact ? 0 : cardRadius,
                            floatTop: compact ? 41 : 46)
        let headers = setting("paneHeaderVisibility") as? String
            ?? (((setting("showsPaneHeaders") as? NSNumber)?.boolValue ?? true) ? "always" : "never")
        if headers != "always" { chrome.header = 0 }
        let vertical = setting("tabStyle") as? String == "vertical"
        let toolbar = setting("toolbarBehavior") as? String ?? "alwaysShow"
        if vertical || toolbar != "alwaysShow" { chrome.top = edge }
        chrome.floatTop = chrome.top
        if vertical && toolbar == "alwaysShow" {
            // The vertical-tab layout leaves a title area above the floating
            // canvas in Rex. This is measured in points, including on Retina.
            chrome.floatTop += 34
        }
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

    /// The outer frame of a Rex floating layer in screen points, with a top-left origin.
    func floatingFrame(_ layer: CGRect, in window: CGRect) -> CGRect {
        let width = window.width - left - right
        let height = window.height - floatTop - bottom
        return CGRect(x: window.minX + left + layer.minX * width,
                      y: window.minY + floatTop + layer.minY * height,
                      width: layer.width * width, height: layer.height * height)
    }

    /// Capture the sides grabbed at mouse-down, not the ones under the pointer later in the drag.
    func resizeGrip(at point: CGPoint, placement: Placement, in window: CGRect) -> ResizeGrip? {
        guard placement.floating, let layer = placement.layerBounds, let id = placement.layerID else { return nil }
        let frame = floatingFrame(layer, in: window)
        guard frame.contains(point), !frame.insetBy(dx: 8, dy: 8).contains(point) else { return nil }
        let horizontal = point.x < frame.minX + 8 ? -1 : (point.x >= frame.maxX - 8 ? 1 : 0)
        let vertical = point.y < frame.minY + 8 ? -1 : (point.y >= frame.maxY - 8 ? 1 : 0)
        // The leaf gets its share of the layer, less its split gaps, border and header.
        let g = placement.gapOffset.size, r = placement.rect
        guard r.width > 0, r.height > 0 else { return nil }
        // body() rounds each edge separately; allow one point in the leaf before scaling to its layer.
        let rounding: CGFloat = 1
        let minimum = CGSize(width: max(96, (Self.minimumBody.width + rounding + 2 - g.width * gap) * layer.width / r.width),
                             height: max(104, (Self.minimumBody.height + rounding + max(header, 1) + 1 - g.height * gap) * layer.height / r.height))
        return ResizeGrip(id: id, bounds: layer, canvas: CGSize(width: window.width - left - right,
                          height: window.height - floatTop - bottom), minimum: minimum, horizontal: horizontal, vertical: vertical)
    }

    /// The block's body in screen points (top-left origin), inside `window`.
    func body(_ placement: Placement, in window: CGRect) -> CGRect? {
        let top = placement.floating ? floatTop : self.top
        let width = window.width - left - right
        let height = window.height - top - bottom
        let fraction = placement.rect
        let gapOffset = placement.gapOffset
        // Gap corrections are signed. CGRect's minX/width accessors standardize negative sizes.
        let x = window.minX + left + fraction.minX * width + gapOffset.origin.x * gap
        let y = window.minY + top + fraction.minY * height + gapOffset.origin.y * gap
        let right = x + fraction.width * width + gapOffset.size.width * gap
        let bottom = y + fraction.height * height + gapOffset.size.height * gap
        let border: CGFloat = radius > 0 || placement.floating ? 1 : 0
        let bodyX = x.rounded() + border
        let bodyY = y.rounded() + max(header, border)
        let body = CGRect(x: bodyX, y: bodyY, width: right.rounded() - border - bodyX,
                          height: bottom.rounded() - border - bodyY)
        guard body.size.width >= Self.minimumBody.width, body.size.height >= Self.minimumBody.height else { return nil }
        for layer in placement.upperLayers {
            if !floatingFrame(layer, in: window).intersection(body).isEmpty { return nil }
        }
        return body
    }
}

/// A drag of a floating pane by its header. Rex reports the layer's new place
/// only once the drag ends, so the browser follows the pointer until then.
struct FloatDrag {
    /// `Placement.rect` when the drag began; a different rect means Rex has landed it.
    let rect: CGRect
    /// The browser's window frame and the pointer when the drag began, in AppKit screen points.
    let frame: NSRect
    let mouse: NSPoint
    var released: Date?

    /// Starts a drag when `mouse` is on the header above `frame`, clear of the
    /// outer edge where Rex resizes the float.
    static func start(at mouse: NSPoint, frame: NSRect, header: CGFloat, rect: CGRect) -> FloatDrag? {
        let edge: CGFloat = 7
        guard header > edge else { return nil }
        let grip = NSRect(x: frame.minX + edge, y: frame.maxY, width: frame.width - 2 * edge, height: header - edge)
        guard grip.contains(mouse) else { return nil }
        return FloatDrag(rect: rect, frame: frame, mouse: mouse)
    }

    func frame(at mouse: NSPoint) -> NSRect {
        frame.offsetBy(dx: (mouse.x - self.mouse.x).rounded(), dy: (mouse.y - self.mouse.y).rounded())
    }
}

struct ResizeGrip {
    let id: String, bounds: CGRect, canvas: CGSize, minimum: CGSize
    let horizontal: Int, vertical: Int

    func resized(dx: CGFloat, dy: CGFloat) -> CGRect {
        func span(_ start: CGFloat, _ size: CGFloat, _ side: Int, _ delta: CGFloat, _ minimum: CGFloat) -> (CGFloat, CGFloat) {
            let end = start + size
            let limit = min(size, minimum)
            if side < 0 { return (max(0, min(start + delta, end - limit)), end) }
            if side > 0 { return (start, min(1, max(end + delta, start + limit))) }
            return (start, end)
        }
        let (left, right) = span(bounds.minX, bounds.width, horizontal, dx / canvas.width, minimum.width / canvas.width)
        let (top, bottom) = span(bounds.minY, bounds.height, vertical, dy / canvas.height, minimum.height / canvas.height)
        return CGRect(x: left, y: top, width: right - left, height: bottom - top)
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

    static func above(_ number: CGWindowID) -> [[String: Any]] {
        CGWindowListCopyWindowInfo([.optionOnScreenAboveWindow], number) as? [[String: Any]] ?? []
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

    /// Whether `event` types text: no ⌘ or ⌃, and either no ⌥ or a ⌥ that
    /// makes a character (€ or @ on many layouts). ⌥ with a key that makes
    /// only whitespace, such as ⌥space, stays a shortcut.
    static func types(_ event: NSEvent) -> Bool {
        let flags = event.modifierFlags
        if !flags.isDisjoint(with: [.command, .control]) { return false }
        guard flags.contains(.option) else { return true }
        return event.characters?.unicodeScalars.contains {
            !CharacterSet.whitespacesAndNewlines.contains($0) && !CharacterSet.controlCharacters.contains($0)
        } ?? false
    }

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
    var handleEdgeEvent: ((NSEvent) -> Bool)?

    override func sendEvent(_ event: NSEvent) {
        if handleEdgeEvent?(event) != true { super.sendEvent(event) }
    }

    func forwardPress(_ down: NSEvent, followedBy event: NSEvent) {
        // A control may enter a tracking loop in mouseDown; queue its drag/up first.
        NSApp.postEvent(event, atStart: true)
        super.sendEvent(down)
    }
}

// MARK: - Asking the agent

/// An element picked with React Grab: a short name and the context it copied.
struct Pick {
    let label: String
    let element: String
}

/// Where a question to the agent stands. A failed send goes back to
/// composing, with the reason shown, so Enter tries again.
enum Asking: Equatable {
    case composing
    case sending
    case sent(agent: String)
}

/// The card at the bottom of the page: the picked elements, the question and
/// what became of it. The question is typed here, not in the page, so a page
/// can't send anything to the agent by itself: only Enter in this field does.
final class Composer: NSView {
    let chips = NSStackView()
    let field = NSTextField()
    let send: ToolButton
    let note = NSTextField(labelWithString: "")
    override var isFlipped: Bool { true }

    init() {
        send = ToolButton(symbol: "arrow.up.circle.fill", label: "Send", target: nil,
                          action: #selector(Browser.sendAsk))
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 12
        layer?.cornerCurve = .continuous
        layer?.borderWidth = 1
        layer?.shadowOpacity = 0.12
        layer?.shadowRadius = 14
        layer?.shadowOffset = CGSize(width: 0, height: -4)
        chips.orientation = .horizontal
        chips.spacing = 6
        chips.alignment = .centerY
        addSubview(chips)
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: 13)
        field.usesSingleLineMode = true
        field.cell?.isScrollable = true
        field.lineBreakMode = .byTruncatingHead
        addSubview(field)
        send.image = NSImage(systemSymbolName: "arrow.up.circle.fill", accessibilityDescription: "Send")?
            .withSymbolConfiguration(.init(pointSize: 18, weight: .regular))
        addSubview(send)
        note.font = .systemFont(ofSize: 11)
        note.lineBreakMode = .byTruncatingTail
        addSubview(note)
        applyAppearance()
    }

    required init?(coder: NSCoder) { nil }

    func applyAppearance() {
        effectiveAppearance.performAsCurrentDrawingAppearance {
            layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
            layer?.borderColor = NSColor.labelColor.withAlphaComponent(0.1).cgColor
            layer?.shadowColor = NSColor.black.cgColor
        }
    }

    /// The height the card needs at `width`.
    var preferredHeight: CGFloat { 10 + 24 + 8 + 26 + (note.stringValue.isEmpty ? 0 : 18) + 10 }

    override func layout() {
        super.layout()
        let width = bounds.width
        chips.frame = NSRect(x: 12, y: 10, width: width - 24, height: 24)
        field.frame = NSRect(x: 14, y: 10 + 24 + 8 + 4, width: width - 14 - 44, height: 18)
        send.frame = NSRect(x: width - 40, y: 10 + 24 + 8 - 1, width: 30, height: 28)
        note.frame = NSRect(x: 14, y: 10 + 24 + 8 + 26 + 2, width: width - 28, height: 16)
    }
}

// MARK: - Browser

final class Browser: NSObject, WKNavigationDelegate, WKUIDelegate, WKDownloadDelegate, NSSearchFieldDelegate,
    WKScriptMessageHandler {
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
    /// Hide the address bar until the pointer is at the top of the page.
    /// Saved by `leaf browser bar`, read from `LEAF_BROWSER_BAR`.
    var minimalBar = false
    /// The bar is on screen. When minimal, this follows the pointer.
    var addressBarShown = true
    /// When the pointer left the top, hide the bar after this moment.
    var barHideAt: Date?
    /// Last text read from the bar setting, so an unchanged file does no work.
    var barModeStamp = ""

    // Layout tracking
    var chrome = Chrome.current()
    var view: [String: Any]?
    var placement: Placement?
    var rex: RexWindow?
    var shown = false
    var shownSession: String?
    var sessionPoll: Timer?
    var sessionIsShown: Bool { shownSession == nil || shownSession == session }
    var lastFrame = NSRect.zero
    var resizeStart: (grip: ResizeGrip, mouse: CGPoint, down: NSEvent)?
    var resizingLayer: String?
    // At most one completed size per layer, followed by the current drag's latest preview.
    var pendingResizes: [(id: String, bounds: CGRect, isFinal: Bool)] = []
    var resizeInFlight: String?
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
    /// The start of a multi-key binding, and whether it was typed in a field.
    var pendingPrefix: (keys: [Chord], inField: Bool)?
    /// Whether a text field on the page has focus, as `Browser.focusScript` reports it.
    var pageEditing = false
    var forwardedAt = Date.distantPast
    var clickMonitor: Any?
    var dragMonitor: Any?
    var floatDrag: FloatDrag?
    var lastExternalClick: (point: NSPoint, time: TimeInterval)?
    var focusRequestedByActivation: TimeInterval?
    var reclaimAfterRefresh: TimeInterval?
    /// Focus asked for before the window was first placed.
    var focusWhenShown = false
    var ticks = 0
    /// Rex's palette or another of its windows is over the pane.
    var coveredByRex = false

    // Page
    var failedURL: URL?
    var errorNavigation: WKNavigation?
    /// The start page is showing; it has no address of its own.
    var onHome = false
    var homeNavigation: WKNavigation?
    var homeLoaded = false
    /// Local web servers found for the start page, as JSON for its script.
    var homeServers: String?
    var titleSent = ""
    var observers: [NSKeyValueObservation] = []
    var downloads: [WKDownload: URL] = [:]

    // Pointing at the page and asking the agent
    let point: ToolButton
    let close: ToolButton
    let composer = Composer()
    var picks: [Pick] = []
    var pickedPage = (url: "", title: "")
    var asking = Asking.composing
    var composerShown = false
    var pointing = false
    /// The last mouse or key event delivered to this window.
    var lastInputAt = Date.distantPast
    /// The agent a question would go to. leaf passes the picked one at launch;
    /// `askWhichAgent` refreshes it, since the pick or the open panes change.
    var agentName: String = {
        let name = ProcessInfo.processInfo.environment["LEAF_AGENT"] ?? ""
        return name.isEmpty ? "your agent" : name
    }()

    static let barHeight: CGFloat = 40
    /// How close to the top, in points, the pointer has to be before a hidden bar returns.
    static let revealHeight: CGFloat = 28

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
        point = ToolButton(symbol: "cursorarrow.rays", label: "Point", target: nil, action: #selector(Browser.togglePoint))
        close = ToolButton(symbol: "xmark", label: "Close Pane (⌘W)", target: nil, action: #selector(Browser.closePane))
        super.init()
        for button in [back, forward, reload, point, close, composer.send] { button.target = self }
        // React Grab runs in the page's own world, where it can read React's
        // component tree; its messages only ever add picks to the composer.
        web.configuration.userContentController.add(self, name: "leaf")
        web.configuration.userContentController.addUserScript(
            WKUserScript(source: Browser.focusScript, injectionTime: .atDocumentStart, forMainFrameOnly: true))
        build()
    }

    /// Tells this helper whether the page's focus is somewhere text is typed,
    /// so plain keys typed there stay with the page (see `handleRexKey`).
    /// A frame with focus counts, since its fields can't be seen from here, and
    /// so does a focused custom element whose shadow root is closed.
    /// Focus is checked again on keys and clicks, since a field removed from
    /// the page takes focus with it without a `focusout`, and on `pageshow`,
    /// since a page back from the back-forward cache keeps its own state.
    static let focusScript = """
        (() => {
          const plain = ["button", "checkbox", "color", "file", "hidden", "image", "radio", "range", "reset", "submit"];
          const typedIn = (el) => !!el && (el.isContentEditable || el.tagName === "TEXTAREA" ||
            el.tagName === "SELECT" || el.tagName === "IFRAME" ||
            (el.tagName === "INPUT" && !plain.includes((el.type || "").toLowerCase())) ||
            (el.tagName.includes("-") && !el.shadowRoot));
          let last;
          const report = (force) => {
            let el = document.activeElement;
            while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
            const now = typedIn(el);
            if (force === true || now !== last) {
              last = now;
              window.webkit.messageHandlers.leaf.postMessage({ editing: now });
            }
          };
          for (const type of ["focusin", "keydown", "pointerdown"]) document.addEventListener(type, report, true);
          document.addEventListener("focusout", () => setTimeout(report, 0), true);
          window.addEventListener("pageshow", () => report(true));
        })();
        """

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
        // Transient: part of Rex's window, so Mission Control hides it rather
        // than laying it out as a window of its own.
        window.collectionBehavior = [.transient, .fullScreenAuxiliary, .ignoresCycle]
        window.title = "Leaf Browser"
        window.animationBehavior = .none

        root.wantsLayer = true
        root.layer?.masksToBounds = true
        root.layer?.cornerCurve = .continuous
        window.contentView = root
        window.handleEdgeEvent = { [weak self] event in self?.handleEdgeEvent(event) ?? false }

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
        // The page fills the pane. The bar floats over its top when shown,
        // so revealing it doesn't shove the page down.
        root.addSubview(bar, positioned: .above, relativeTo: web)
        root.addSubview(findBar, positioned: .above, relativeTo: web)
        window.initialFirstResponder = web

        bar.addSubview(point)
        bar.addSubview(close)
        composer.isHidden = true
        nameAgent()
        composer.field.delegate = self
        composer.field.target = self
        composer.field.action = #selector(sendAsk)
        root.addSubview(composer)

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
        pollBarMode(animated: false)
        layout()
    }

    /// Light or dark like Rex, the page's `prefers-color-scheme` included.
    func followRexAppearance() {
        let wanted = Chrome.appearance()
        guard NSApp.appearance?.name != wanted?.name else { return }
        NSApp.appearance = wanted
        applyAppearance()
    }

    func applyAppearance() {
        root.effectiveAppearance.performAsCurrentDrawingAppearance {
            root.layer?.backgroundColor = NSColor.textBackgroundColor.cgColor
            bar.layer?.backgroundColor = NSColor.textBackgroundColor.cgColor
            pill.layer?.backgroundColor = NSColor.labelColor.withAlphaComponent(0.06).cgColor
            progress.backgroundColor = NSColor.controlAccentColor.cgColor
        }
        bar.needsDisplay = true
        findBar.needsDisplay = true
        composer.applyAppearance()
        updatePoint()
    }

    /// The address is being edited, or find is open: the bar stays up either way.
    var barPinned: Bool { editingAddress || findShown }

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
        pill.frame = NSRect(x: pillX, y: top, width: max(60, width - pillX - 10 - 2 * (size + 2)), height: size)
        point.frame = NSRect(x: pill.frame.maxX + 4, y: top, width: size, height: size)
        close.frame = NSRect(x: point.frame.maxX + 2, y: top, width: size, height: size)
        badge.frame = NSRect(x: 9, y: (size - 14) / 2, width: 14, height: 14)
        let fieldHeight: CGFloat = 17
        address.frame = NSRect(x: 29, y: (size - fieldHeight) / 2, width: pill.frame.width - 37, height: fieldHeight)
        layoutProgress(animated: false)

        // The always-on bar keeps its row, as before. A minimal bar floats
        // over the page, so hovering it into view doesn't shove the page down.
        let findHeight: CGFloat = findShown ? 36 : 0
        let inFlow = addressBarShown && !minimalBar
        let aboveFind = (findShown && addressBarShown) ? barHeight : (inFlow ? barHeight : 0)
        findBar.frame = NSRect(x: 0, y: height - aboveFind - findHeight, width: width, height: findHeight)
        findField.frame = NSRect(x: 10, y: 6, width: min(320, width - 56), height: 24)
        if let done = findBar.subviews.first(where: { $0.identifier?.rawValue == "done" }) {
            done.frame = NSRect(x: findField.frame.maxX + 6, y: 4, width: 28, height: 28)
        }
        web.frame = NSRect(x: 0, y: 0, width: width, height: max(0, height - aboveFind - findHeight))
        layoutComposer()
    }

    /// The saved address-bar mode, or `full` when the file isn't there yet.
    func currentBarFile() -> String {
        guard let path = ProcessInfo.processInfo.environment["LEAF_BROWSER_BAR"], !path.isEmpty,
              let text = try? String(contentsOfFile: path, encoding: .utf8) else { return "full" }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? "full" : trimmed
    }

    /// Picks up `leaf browser bar` while this pane is open.
    func pollBarMode(animated: Bool) {
        let text = currentBarFile()
        guard text != barModeStamp else { return }
        barModeStamp = text
        let minimal = text == "minimal"
        guard minimal != minimalBar else { return }
        minimalBar = minimal
        syncAddressBar(animated: animated)
        // The always-on bar takes a row; the minimal one doesn't. Reflow
        // even when the bar was already visible and only the mode changed.
        layout()
    }

    func syncAddressBar(animated: Bool) {
        let show = !minimalBar || barPinned || (shown && pointerInTopRegion())
        setAddressBarShown(show, animated: animated)
    }

    /// Shows the bar while the pointer is in the top strip, and hides it
    /// shortly after the pointer leaves. Does nothing when the bar is always on.
    func updateAddressBarHover() {
        guard minimalBar, shown else { return }
        if barPinned || pointerInTopRegion() {
            barHideAt = nil
            if !addressBarShown { setAddressBarShown(true, animated: true) }
        } else if addressBarShown {
            if barHideAt == nil { barHideAt = Date().addingTimeInterval(0.2) }
            if let barHideAt, Date() >= barHideAt {
                self.barHideAt = nil
                setAddressBarShown(false, animated: true)
            }
        }
    }

    /// Whether the pointer is over this pane's top strip. The page keeps the
    /// clicks there; only the position is read, so the strip isn't a view.
    func pointerInTopRegion() -> Bool {
        guard shown else { return false }
        let mouse = NSEvent.mouseLocation
        let frame = window.frame
        guard mouse.x >= frame.minX, mouse.x < frame.maxX, mouse.y >= frame.minY, mouse.y < frame.maxY else {
            return false
        }
        let fromTop = frame.maxY - mouse.y
        let zone = addressBarShown ? Browser.barHeight : Browser.revealHeight
        return fromTop >= 0 && fromTop <= zone
    }

    func setAddressBarShown(_ show: Bool, animated: Bool) {
        let changed = show != addressBarShown || bar.isHidden == show
        addressBarShown = show
        guard changed else { return }
        if !animated {
            bar.isHidden = !show
            bar.alphaValue = show ? 1 : 0
            layout()
            return
        }
        if show {
            bar.alphaValue = 0
            bar.isHidden = false
            layout()
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.14
                context.timingFunction = CAMediaTimingFunction(name: .easeOut)
                bar.animator().alphaValue = 1
            }
        } else {
            // The page takes the bar's row as the bar fades, in the mode
            // where the bar was part of the layout.
            layout()
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.12
                bar.animator().alphaValue = 0
            } completionHandler: { [weak self] in
                guard let self, !self.addressBarShown else { return }
                self.bar.isHidden = true
            }
        }
    }

    /// Brings the bar back for the address field or find, even while minimal.
    func revealAddressBar() {
        barHideAt = nil
        guard minimalBar else { return }
        setAddressBarShown(true, animated: false)
    }

    func layoutComposer() {
        let width = min(560, root.bounds.width - 24)
        let height = composer.preferredHeight
        composer.frame = NSRect(x: (root.bounds.width - width) / 2, y: 12, width: width, height: height)
        composer.needsLayout = true
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
        let url = onHome ? nil : (failedURL ?? web.url)
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
        if control === composer.field, selector == #selector(NSResponder.cancelOperation(_:)) {
            if asking != .sending { closeComposer() }
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
        let label = onHome ? "browser" : title.isEmpty ? (web.url?.host ?? "browser") : String(title.prefix(48))
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
        if onHome { showHome() } else if let failed = failedURL { load(failed) } else { web.reload() }
    }
    @objc func hardReload() { web.reloadFromOrigin() }
    @objc func focusAddress() {
        revealAddressBar()
        window.makeFirstResponder(address)
    }
    @objc func zoomIn() { web.pageZoom = min(3, web.pageZoom + 0.1) }
    @objc func zoomOut() { web.pageZoom = max(0.3, web.pageZoom - 0.1) }
    @objc func zoomReset() { web.pageZoom = 1 }

    @objc func closePane() {
        let session = session, block = block
        window.orderOut(nil)
        Rex.run(["block", "close", "--session", session, block])
        shutDown()
    }

    // MARK: Find

    @objc func showFind() {
        revealAddressBar()
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

    @objc func findChanged() { find(backwards: false) }
    @objc func findNext() { if findShown { find(backwards: false) } else { showFind() } }
    @objc func findPrevious() { if findShown { find(backwards: true) } else { showFind() } }

    func find(backwards: Bool) {
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
        if navigation !== homeNavigation { onHome = false; homeLoaded = false }
        if findShown { closeFind() }
    }

    func webView(_ webView: WKWebView, didCommit navigation: WKNavigation!) {
        // A new document: React Grab and the focused field went with the old one.
        pointing = false
        pageEditing = false
        updatePoint()
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        if navigation === homeNavigation {
            homeLoaded = true
            listServers()
        }
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

    // MARK: Start page

    /// Shows the start page: leaf's mark, a hint, and the web servers
    /// running on this Mac, filled in once they're found.
    func showHome() {
        onHome = true
        homeLoaded = false
        homeServers = nil
        failedURL = nil
        homeNavigation = web.loadHTMLString(Browser.homePage(agent: agentName), baseURL: nil)
        showURL()
        Browser.findServers { [weak self] json in
            guard let self, self.onHome else { return }
            self.homeServers = json
            self.listServers()
        }
    }

    /// Hands the servers found to the start page once both are ready.
    func listServers() {
        guard onHome, homeLoaded, let json = homeServers else { return }
        web.evaluateJavaScript("window.leafServers && window.leafServers(\(json))")
    }

    static func homePage(agent: String) -> String {
        """
        <!doctype html><meta charset="utf-8"><meta name="color-scheme" content="light dark"><title>New page</title>
        <style>
        :root{--fg:#17171a;--muted:#8b8b90;--line:rgba(128,128,128,.16);--hl:#e8b93a}
        @media (prefers-color-scheme:dark){:root{--fg:#efefec;--muted:#8e8e93;--line:rgba(255,255,255,.09);--hl:#f3d35f}}
        html,body{height:100%;margin:0}
        body{display:grid;place-items:center;background:Canvas;color:var(--fg);font:13px/1.55 -apple-system,system-ui;-webkit-font-smoothing:antialiased;-webkit-user-select:none;cursor:default}
        main{width:min(400px,84vw);display:flex;flex-direction:column;align-items:center;margin-top:-8vh}
        .mark{width:40px;height:40px;margin-bottom:22px}
        .mark rect{fill:var(--fg)} .mark .l{stroke:Canvas} .mark .h{stroke:var(--hl)}
        .hint{color:var(--muted);text-align:center;margin:0}
        kbd{font:11px ui-monospace,SFMono-Regular,Menlo,monospace;padding:1px 5px;border:1px solid var(--line);border-radius:5px;color:var(--fg)}
        #servers{width:100%;margin-top:34px;opacity:0;transition:opacity .25s ease}
        #servers.on{opacity:1}
        .label{font-size:10.5px;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);padding:0 10px 6px}
        a{display:flex;align-items:baseline;gap:12px;padding:7px 10px;border-radius:8px;color:inherit;text-decoration:none}
        a:hover{background:var(--line)}
        .port{font:12px ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--muted);flex:none;min-width:15ch}
        .name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
        </style>
        <main>
          <svg class="mark" viewBox="0 0 32 32" aria-label="leaf"><rect width="32" height="32" rx="8"/>
          <path class="l" d="M8.5 10h15" stroke-width="3.5" stroke-linecap="round"/>
          <path class="h" d="M8.5 16h15" stroke-width="3.5" stroke-linecap="round"/>
          <path class="l" d="M8.5 22h9" stroke-width="3.5" stroke-linecap="round"/></svg>
          <p class="hint">Type an address or search above<br><kbd>⇧⌘C</kbd> points at a page to ask \(escape(agent))</p>
          <section id="servers" hidden><div class="label">Running here</div><div id="list"></div></section>
        </main>
        <script>
        window.leafServers = (servers) => {
          const list = document.getElementById("list"), box = document.getElementById("servers");
          list.replaceChildren(...servers.map(({ port, title }) => {
            const a = document.createElement("a");
            a.href = "http://localhost:" + port + "/";
            const p = document.createElement("span"); p.className = "port"; p.textContent = "localhost:" + port;
            const n = document.createElement("span"); n.className = "name"; n.textContent = title;
            a.append(p, n);
            return a;
          }));
          box.hidden = servers.length === 0;
          requestAnimationFrame(() => box.classList.toggle("on", servers.length > 0));
        };
        </script>
        """
    }

    /// The web servers listening on this Mac for this user: ports from
    /// `lsof`, kept when they answer HTTP with a page. Calls back on the main
    /// queue with a JSON array of {port, title}.
    static func findServers(_ done: @escaping (String) -> Void) {
        Rex.background {
            let task = Process()
            task.executableURL = URL(fileURLWithPath: "/usr/sbin/lsof")
            task.arguments = ["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-u", String(getuid()), "-Fn"]
            let pipe = Pipe()
            task.standardOutput = pipe
            task.standardError = FileHandle.nullDevice
            var ports: [Int] = []
            if (try? task.run()) != nil {
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                task.waitUntilExit()
                for line in String(decoding: data, as: UTF8.self).split(separator: "\n") where line.hasPrefix("n") {
                    // n*:3000, n127.0.0.1:5173, n[::1]:8080
                    guard let colon = line.lastIndex(of: ":"), let port = Int(line[line.index(after: colon)...]),
                          port >= 1024, !ports.contains(port) else { continue }
                    ports.append(port)
                }
            }
            ports.sort()
            let configuration = URLSessionConfiguration.ephemeral
            configuration.timeoutIntervalForRequest = 1.2
            configuration.connectionProxyDictionary = [:]
            let session = URLSession(configuration: configuration)
            let lock = NSLock()
            var found: [(port: Int, title: String)] = []
            let group = DispatchGroup()
            for port in ports.prefix(24) {
                guard let url = URL(string: "http://localhost:\(port)/") else { continue }
                group.enter()
                session.dataTask(with: url) { data, response, _ in
                    defer { group.leave() }
                    guard let http = response as? HTTPURLResponse,
                          (http.value(forHTTPHeaderField: "Content-Type") ?? "").lowercased().contains("text/html"),
                          let data else { return }
                    let title = Browser.pageTitle(String(decoding: data.prefix(65_536), as: UTF8.self))
                    lock.lock()
                    found.append((port, title ?? "Web server"))
                    lock.unlock()
                }.resume()
            }
            group.wait()
            session.invalidateAndCancel()
            let rows = found.sorted { $0.port < $1.port }.prefix(8).map { ["port": $0.port, "title": $0.title] as [String: Any] }
            let json = (try? JSONSerialization.data(withJSONObject: Array(rows))).map { String(decoding: $0, as: UTF8.self) } ?? "[]"
            DispatchQueue.main.async { done(json) }
        }
    }

    /// A page's `<title>`, tidied, from the start of its HTML.
    static func pageTitle(_ html: String) -> String? {
        guard let open = html.range(of: "<title", options: .caseInsensitive),
              let start = html.range(of: ">", range: open.upperBound..<html.endIndex),
              let end = html.range(of: "</title>", options: .caseInsensitive, range: start.upperBound..<html.endIndex)
        else { return nil }
        let title = html[start.upperBound..<end.lowerBound]
            .replacingOccurrences(of: "&amp;", with: "&").replacingOccurrences(of: "&#39;", with: "'")
            .replacingOccurrences(of: "&quot;", with: "\"").replacingOccurrences(of: "&lt;", with: "<")
            .replacingOccurrences(of: "&gt;", with: ">")
            .split(whereSeparator: \.isWhitespace).joined(separator: " ")
        return title.isEmpty ? nil : String(title.prefix(80))
    }

    static func escape(_ text: String) -> String {
        text.replacingOccurrences(of: "&", with: "&amp;").replacingOccurrences(of: "<", with: "&lt;")
            .replacingOccurrences(of: ">", with: "&gt;")
    }

    static func errorPage(title: String, reason: String) -> String {
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

    // MARK: Pointing and asking

    /// React Grab's bundle, unpacked by leaf beside this helper.
    static let grabBundle: String? = {
        guard let executable = Bundle.main.executableURL else { return nil }
        let script = executable.deletingLastPathComponent().appendingPathComponent("react-grab.js")
        return try? String(contentsOf: script, encoding: .utf8)
    }()

    /// Loads React Grab into the page (or uses one the page already has),
    /// hooks its copies into this helper, and starts pointing.
    static func grabSetup(_ bundle: String) -> String {
        """
        (() => {
          if (!window.__REACT_GRAB__) {
            window.__REACT_GRAB_DISABLED__ = true;
            (function () {
        \(bundle)
            }).call(window);
            window.__REACT_GRAB_DISABLED__ = false;
            const grab = globalThis.__REACT_GRAB_MODULE__;
            // No key of its own: ⌘C stays copy, and leaf's ⇧⌘C starts pointing.
            const created = grab.init({ telemetry: false, activationKey: () => false });
            grab.setGlobalApi(created);
            window.__REACT_GRAB__ = window.__REACT_GRAB__ || created;
          }
          const api = window.__REACT_GRAB__;
          const post = (message) => window.webkit.messageHandlers.leaf.postMessage(message);
          let quoted = null;
          api.registerPlugin({
            name: "leaf",
            hooks: {
              onActivate: () => post({ pointing: true }),
              onDeactivate: () => post({ pointing: false }),
              transformCopyContent: (content) => { quoted = content; return content; },
              onCopySuccess: (elements, content) => {
                const element = quoted ?? content;
                quoted = null;
                // A comment typed in React Grab comes first, on its own line.
                const comment = content !== element && content.endsWith("\\n" + element)
                  ? content.slice(0, content.length - element.length - 1) : "";
                const first = elements[0];
                const name = first ? (api.getDisplayName(first) || "<" + first.tagName.toLowerCase() + ">") : "element";
                post({ element, comment, label: elements.length > 1 ? elements.length + " elements" : name });
              },
            },
          });
          window.__leafGrab = api;
          api.activate();
          return true;
        })();
        """
    }

    func nameAgent() {
        point.toolTip = "Point at part of the page to ask \(agentName) about it (⇧⌘C)"
        composer.field.placeholderString = "Ask \(agentName) about this…"
    }

    /// Writes one request line for leaf; false when leaf is gone.
    func tellLeaf(_ request: [String: Any]) -> Bool {
        guard let data = try? JSONSerialization.data(withJSONObject: request) else { return false }
        do {
            try FileHandle.standardOutput.write(contentsOf: data + Data("\n".utf8))
            return true
        } catch {
            return false
        }
    }

    /// Asks leaf which agent a question would go to now; `replied` names it.
    func askWhichAgent() {
        _ = tellLeaf(["agent": [String: Any]()])
    }

    @objc func togglePoint() {
        askWhichAgent()
        window.makeFirstResponder(web)
        let toggle = "window.__leafGrab ? (window.__leafGrab.toggle(), true) : false"
        web.evaluateJavaScript(toggle) { [weak self] result, _ in
            guard let self, result as? Bool != true else { return }
            guard let bundle = Browser.grabBundle else {
                self.showComposer(note: "Pointing isn't available: leaf's copy of React Grab is missing. Reinstall leaf.",
                                  failed: true)
                return
            }
            self.web.evaluateJavaScript(Browser.grabSetup(bundle)) { _, error in
                if let error {
                    self.showComposer(note: "This page didn't let leaf point at it (\(error.localizedDescription)).",
                                      failed: true)
                }
            }
        }
    }

    func updatePoint() {
        point.contentTintColor = pointing ? .controlAccentColor : .secondaryLabelColor
    }

    func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
        guard message.frameInfo.isMainFrame, let body = message.body as? [String: Any] else { return }
        if let editing = body["editing"] as? Bool {
            pageEditing = editing
            return
        }
        if let active = body["pointing"] as? Bool {
            pointing = active
            updatePoint()
            return
        }
        // A pick follows the person's own click or key in this window; a page
        // can post at any time but can't make those.
        guard Date().timeIntervalSince(lastInputAt) < 2, let element = body["element"] as? String,
              !element.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let label = (body["label"] as? String).map { String($0.prefix(40)) } ?? "element"
        if picks.isEmpty || asking != .composing {
            picks = []
            pickedPage = (web.url?.absoluteString ?? "", web.title ?? "")
            composer.field.stringValue = ""
            askWhichAgent()
        }
        picks.append(Pick(label: label, element: String(element.prefix(20_000))))
        let comment = (body["comment"] as? String ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        if !comment.isEmpty && composer.field.stringValue.isEmpty { composer.field.stringValue = comment }
        asking = .composing
        showComposer(note: nil, failed: false)
    }

    func showComposer(note: String?, failed: Bool) {
        renderChips()
        composer.note.stringValue = note ?? ""
        composer.note.textColor = failed ? .systemRed : .secondaryLabelColor
        composer.field.isEnabled = asking != .sending
        composer.send.isEnabled = asking != .sending && !picks.isEmpty
        layoutComposer()
        if !composerShown {
            composerShown = true
            composer.isHidden = false
            composer.alphaValue = 0
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.16
                composer.animator().alphaValue = 1
            }
        }
        if asking == .composing {
            window.makeFirstResponder(composer.field)
            composer.field.currentEditor()?.moveToEndOfDocument(nil)
        }
    }

    func renderChips() {
        composer.chips.arrangedSubviews.forEach { $0.removeFromSuperview() }
        for (index, pick) in picks.enumerated() {
            let chip = NSButton(title: pick.label, target: self, action: #selector(removePick(_:)))
            chip.tag = index
            chip.isBordered = false
            chip.font = .monospacedSystemFont(ofSize: 11, weight: .medium)
            chip.image = NSImage(systemSymbolName: "xmark", accessibilityDescription: "Remove")?
                .withSymbolConfiguration(.init(pointSize: 8, weight: .bold))
            chip.imagePosition = .imageTrailing
            chip.contentTintColor = .secondaryLabelColor
            chip.toolTip = String(pick.element.prefix(400))
            chip.wantsLayer = true
            chip.layer?.cornerRadius = 6
            chip.effectiveAppearance.performAsCurrentDrawingAppearance {
                chip.layer?.backgroundColor = NSColor.controlAccentColor.withAlphaComponent(0.12).cgColor
            }
            chip.translatesAutoresizingMaskIntoConstraints = false
            chip.heightAnchor.constraint(equalToConstant: 22).isActive = true
            let width = (pick.label as NSString).size(withAttributes: [.font: chip.font as Any]).width + 28
            chip.widthAnchor.constraint(equalToConstant: ceil(width)).isActive = true
            composer.chips.addArrangedSubview(chip)
        }
        let more = NSButton(title: "Point at more", target: self, action: #selector(togglePoint))
        more.isBordered = false
        more.font = .systemFont(ofSize: 11)
        more.contentTintColor = .tertiaryLabelColor
        more.image = NSImage(systemSymbolName: "plus", accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 9, weight: .semibold))
        more.imagePosition = .imageLeading
        composer.chips.addArrangedSubview(more)
    }

    @objc func removePick(_ sender: NSButton) {
        guard picks.indices.contains(sender.tag), asking != .sending else { return }
        picks.remove(at: sender.tag)
        if picks.isEmpty { closeComposer() } else { showComposer(note: nil, failed: false) }
    }

    func closeComposer() {
        picks = []
        asking = .composing
        composer.field.stringValue = ""
        composerShown = false
        composer.isHidden = true
        if pointing { web.evaluateJavaScript("window.__leafGrab && window.__leafGrab.deactivate()") }
        window.makeFirstResponder(web)
    }

    /// Hands the question to leaf, which sends it to the agent.
    @objc func sendAsk() {
        guard asking != .sending, !picks.isEmpty else { return }
        let ask: [String: Any] = [
            "question": composer.field.stringValue,
            "url": String(pickedPage.url.prefix(2000)),
            "title": String(pickedPage.title.prefix(300)),
            "elements": picks.map(\.element),
        ]
        guard tellLeaf(["ask": ask]) else {
            showComposer(note: "leaf stopped listening, so nothing was sent. Close this pane and open the browser again.",
                         failed: true)
            return
        }
        asking = .sending
        if pointing { web.evaluateJavaScript("window.__leafGrab && window.__leafGrab.deactivate()") }
        showComposer(note: "Sending to \(agentName)…", failed: false)
    }

    /// leaf's answer to a question, one JSON line.
    func replied(_ line: Data) {
        guard let object = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] else { return }
        if let agent = object["agent"] as? [String: Any] {
            let name = agent["name"] as? String ?? ""
            agentName = name.isEmpty ? "your agent" : name
            nameAgent()
            return
        }
        guard asking == .sending else { return }
        if let sent = object["sent"] as? [String: Any] {
            let agent = sent["agent"] as? String ?? agentName
            agentName = agent
            nameAgent()
            asking = .sent(agent: agent)
            showComposer(note: "Sent to \(agent). Its answer is in the \(agent) pane.", failed: false)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.6) { [weak self] in
                guard let self, case .sent = self.asking else { return }
                self.closeComposer()
            }
        } else if let failed = object["failed"] as? [String: Any] {
            let message = failed["message"] as? String ?? "Sending failed for a reason leaf didn't give."
            var copied = false
            if let prompt = failed["prompt"] as? String, !prompt.isEmpty {
                NSPasteboard.general.clearContents()
                copied = NSPasteboard.general.setString(prompt, forType: .string)
            }
            // Still composing: fix what's wrong and press Enter again.
            asking = .composing
            let said = message.contains("clipboard") || !copied
            showComposer(note: said ? message : message + " The question was copied to the clipboard.", failed: true)
        }
    }

    func listenToLeaf() {
        var pending = Data()
        FileHandle.standardInput.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            if chunk.isEmpty {
                handle.readabilityHandler = nil
                return
            }
            pending.append(chunk)
            while let end = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let line = Data(pending[pending.startIndex..<end])
                pending.removeSubrange(pending.startIndex...end)
                DispatchQueue.main.async { self?.replied(line) }
            }
        }
    }

    // MARK: Following the pane

    func handleEdgeEvent(_ event: NSEvent) -> Bool {
        switch event.type {
        case .leftMouseDown:
            resizeStart = nil
            resizingLayer = nil
            guard shown, let placement, placement.floating, let rex, let bounds = WindowServer.bounds(of: rex.number),
                  let primary = NSScreen.screens.first else { return false }
            let mouse = window.convertPoint(toScreen: event.locationInWindow)
            let point = CGPoint(x: mouse.x, y: primary.frame.height - mouse.y)
            guard let grip = chrome.resizeGrip(at: point, placement: placement, in: bounds) else { return false }
            resizeStart = (grip, mouse, event)
            return true
        case .leftMouseDragged, .leftMouseUp:
            guard let start = resizeStart else { return false }
            let mouse = window.convertPoint(toScreen: event.locationInWindow)
            let dx = mouse.x - start.mouse.x, dy = start.mouse.y - mouse.y
            if resizingLayer == nil {
                let tiny = max(abs(dx), abs(dy)) < 3
                let parallel = (start.grip.horizontal == 0 && abs(dx) > abs(dy)) ||
                               (start.grip.vertical == 0 && abs(dy) > abs(dx))
                if tiny && event.type != .leftMouseUp { return true }
                if tiny || parallel {
                    // Ordinary clicks and scrolling along the edge still belong to WebKit.
                    resizeStart = nil
                    window.forwardPress(start.down, followedBy: event)
                    return true
                }
                resizingLayer = start.grip.id
            }
            let released = event.type == .leftMouseUp
            pendingResizes.removeAll { !$0.isFinal || (released && $0.id == start.grip.id) }
            pendingResizes.append((start.grip.id, start.grip.resized(dx: dx, dy: dy), released))
            if released { resizeStart = nil; resizingLayer = nil }
            sendPendingResize()
            return true
        default: return false
        }
    }

    /// Send queued resizes one at a time, oldest first. Keep mouse-up bounds until
    /// delivery or a later mouse-up on the same layer.
    func sendPendingResize() {
        guard resizeInFlight == nil, !pendingResizes.isEmpty else { return }
        let next = pendingResizes.removeFirst()
        resizeInFlight = next.id
        let r = next.bounds
        Rex.background {
            _ = Rex.call("session.resize_layer", ["session_id": self.session, "layer_id": next.id,
                         "bounds": [r.minX, r.minY, r.width, r.height]])
            DispatchQueue.main.async {
                self.resizeInFlight = nil
                if self.pendingResizes.isEmpty { self.refresh() } else { self.sendPendingResize() }
            }
        }
    }

    func start() {
        // Switching sessions in the Rex sidebar need not change this session's server-side layout.
        shownSession = Chrome.shownSession()
        let poll = Timer(timeInterval: 0.03, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.shownSession = Chrome.shownSession()
            if !self.sessionIsShown {
                self.focusWhenShown = false
                self.focusRequestedByActivation = nil
                self.reclaimAfterRefresh = nil
                self.hide()
            }
        }
        sessionPoll = poll
        RunLoop.main.add(poll, forMode: .common)
        clickMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown]) {
            [weak self] event in
            guard let self, let point = event.cgEvent?.location, let primary = NSScreen.screens.first else { return }
            self.lastExternalClick = (CGPoint(x: point.x, y: primary.frame.height - point.y), event.timestamp)
            self.floatDrag = nil
            if event.type == .leftMouseDown, self.shown, let placement = self.placement, placement.floating {
                self.floatDrag = FloatDrag.start(at: NSEvent.mouseLocation, frame: self.lastFrame,
                                                 header: self.chrome.header, rect: placement.rect)
            }
        }
        dragMonitor = NSEvent.addGlobalMonitorForEvents(matching: [.leftMouseDragged, .leftMouseUp]) {
            [weak self] event in
            self?.followFloatDrag(to: NSEvent.mouseLocation, released: event.type == .leftMouseUp)
        }
        Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in self?.follow() }
            .tolerance = 0.002
        Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            if getppid() != self.parent { self.shutDown() }
            self.chrome = Chrome.current()
            self.followRexAppearance()
            self.pollBarMode(animated: true)
            self.refresh()
        }
        let center = NSWorkspace.shared.notificationCenter
        center.addObserver(forName: NSWorkspace.didActivateApplicationNotification, object: nil, queue: .main) {
            [weak self] note in self?.applicationActivated(note)
        }
        followRexAppearance()
        watchEvents()
        listenToLeaf()
        askWhichAgent()
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
                if missing >= 2 { shutDown() }
            }
            let next = SessionView.placement(view, block: block)
            // Rex animates panes into place. Following that animation would
            // lag it, so step out of the way and come back once it settles.
            let followsLayer = next?.followsLayer(from: placement) == true
            if !followsLayer && (next?.rect != placement?.rect || next?.floating != placement?.floating || next?.gapOffset != placement?.gapOffset) {
                settleUntil = Date().addingTimeInterval(0.28)
                hide()
            }
            placement = next
            syncFocus()
        }
        if refreshAgain {
            refreshAgain = false
            refresh()
        } else if let requestedAt = reclaimAfterRefresh {
            reclaimAfterRefresh = nil
            if view != nil { reclaimFocusAfterActivation(at: requestedAt) }
        }
    }

    func requestFocusAfterRefresh(at time: TimeInterval = ProcessInfo.processInfo.systemUptime) {
        reclaimAfterRefresh = time
        refresh()
    }

    func pickRexWindow() -> RexWindow? {
        let windows = WindowServer.rexWindows(WindowServer.list())
        if let current = rex, let same = windows.first(where: { $0.number == current.number }) { return same }
        return windows.first
    }

    /// Moves the page with a header drag Rex is handling, and keeps it above
    /// Rex's window, which the click raised.
    /// `mouse` is in AppKit screen points; `released` is true for the mouse-up.
    func followFloatDrag(to mouse: NSPoint, released: Bool) {
        guard var drag = floatDrag, drag.released == nil, shown, let rex else { return }
        // The release can land between drag events: place the page there first.
        let frame = drag.frame(at: mouse)
        if frame != lastFrame {
            if let primary = NSScreen.screens.first?.frame.height {
                let body = CGRect(x: frame.minX, y: primary - frame.maxY, width: frame.width, height: frame.height)
                updateSiblingLevel(covering: body)
            }
            // Only the origin changes: moving without a redraw keeps up with the pointer.
            window.setFrameOrigin(frame.origin)
            lastFrame = frame
        }
        if window.level == .normal && !directlyAbove(rex.number) {
            window.order(.above, relativeTo: Int(rex.number))
        }
        if released {
            drag.released = Date()
            floatDrag = drag
            refresh()
        }
    }

    func follow() {
        guard sessionIsShown else { focusWhenShown = false; hide(); return }
        if let drag = floatDrag {
            // Hold the dragged frame until Rex reports where the float landed,
            // or briefly after a click that didn't move it.
            let landed = placement?.rect != drag.rect
            let settled = drag.released.map { Date().timeIntervalSince($0) > 0.5 } ?? false
            if !landed && !settled { return }
            floatDrag = nil
        }
        ticks &+= 1
        if ticks % 8 == 0 { pollBarMode(animated: true) }
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
        guard let body = chrome.body(placement, in: bounds) else { hide(); return }
        // Rex's palette and dialogs are windows of their own over the pane;
        // they must stay on top and keep the keyboard, so give way to them.
        if !shown || !watching || ticks % 3 == 0 {
            let covered = rexOverlay(covering: body, of: target)
            if !covered && coveredByRex && rexIsActive {
                // Dismissing a panel may itself select a different block.
                focusWhenShown = false
                focusRequestedByActivation = nil
                requestFocusAfterRefresh()
            }
            coveredByRex = covered
        }
        if coveredByRex { hide(); return }
        let frame = NSRect(x: body.minX.rounded(), y: (primary - body.maxY).rounded(),
                           width: body.width.rounded(), height: body.height.rounded())
        if frame != lastFrame {
            if shown { updateSiblingLevel(covering: body) }
            window.setFrame(frame, display: true)
            lastFrame = frame
        }
        let radius = placement.floating ? Chrome.cardRadius : chrome.radius
        if root.layer?.cornerRadius != radius { root.layer?.cornerRadius = radius }
        if !shown {
            window.alphaValue = 0
            if let app = NSWorkspace.shared.frontmostApplication {
                updateLevel(for: app.processIdentifier, executable: app.executableURL, body: body)
            }
            if window.level == .floating { window.orderFront(nil) }
            else { window.order(.above, relativeTo: Int(target.number)) }
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.12
                window.animator().alphaValue = 1
            }
        } else if window.level == .normal && ticks % 10 == 0 && !directlyAbove(target.number) {
            window.order(.above, relativeTo: Int(target.number))
        }
        shown = true
        if focusWhenShown {
            focusWhenShown = false
            focusRequestedByActivation = nil
            takeFocus()
        } else if let requestedAt = focusRequestedByActivation {
            reclaimFocusAfterActivation(at: requestedAt)
        }
        if shown { updateAddressBarHover() }
    }

    /// Whether another window of Rex's, in front of its main window, overlaps
    /// `body` (screen points, top-left origin): the palette, a dialog, or a
    /// second Rex window.
    func rexOverlay(covering body: CGRect, of rex: RexWindow, windows: [[String: Any]] = WindowServer.list()) -> Bool {
        for item in windows {
            guard let number = item[kCGWindowNumber as String] as? CGWindowID else { continue }
            // The list runs front to back: past Rex's window, nothing covers it.
            if number == rex.number { return false }
            guard item[kCGWindowOwnerPID as String] as? pid_t == rex.pid,
                  let layer = item[kCGWindowLayer as String] as? Int,
                  layer == NSWindow.Level.normal.rawValue || layer == NSWindow.Level.floating.rawValue,
                  let dictionary = item[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: dictionary) else { continue }
            if bounds.intersects(body) { return true }
        }
        return false
    }

    /// Whether only Leaf browser windows lie between this window and Rex's.
    func directlyAbove(_ rexNumber: CGWindowID) -> Bool {
        let mine = CGWindowID(window.windowNumber)
        let above = WindowServer.above(rexNumber)
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

    func stopSidebarTracking() {
        sessionPoll?.invalidate()
        sessionPoll = nil
        if let clickMonitor { NSEvent.removeMonitor(clickMonitor) }
        clickMonitor = nil
        if let dragMonitor { NSEvent.removeMonitor(dragMonitor) }
        dragMonitor = nil
    }

    /// Stops the `rex events` watcher, then exits: every way out goes here.
    func shutDown() -> Never {
        stopSidebarTracking()
        events?.terminationHandler = nil
        events?.terminate()
        exit(0)
    }

    func hide() {
        guard shown else { return }
        floatDrag = nil
        resizeStart = nil
        resizingLayer = nil
        // An accepted mouse-up belongs to its original layer even when the browser is hidden.
        pendingResizes.removeAll { !$0.isFinal }
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
        guard sessionIsShown else { return }
        if focused && !wasFocusedInRex && rexIsActive && reclaimAfterRefresh == nil {
            takeFocus()
        } else if !focused && wasFocusedInRex && NSApp.isActive {
            // Rex moved focus to another pane, by a forwarded key or its API.
            activateRex()
        }
    }

    func updateSiblingLevel(covering body: CGRect) {
        guard let app = NSWorkspace.shared.frontmostApplication, app.processIdentifier != getpid(),
              let executable = app.executableURL, executable == Bundle.main.executableURL else { return }
        updateLevel(for: app.processIdentifier, executable: executable, body: body)
    }

    // Rex raises its normal window before the activation notification arrives.
    func updateLevel(for pid: pid_t, executable: URL?, body: CGRect? = nil,
                     windowsAbove: (CGWindowID) -> [[String: Any]] = WindowServer.above) {
        let sibling = executable != nil && executable == Bundle.main.executableURL
        var raise = pid == rex?.pid || pid == getpid()
        if !raise && sibling, let host = rex {
            let paneBody = body ?? placement.flatMap { chrome.body($0, in: WindowServer.bounds(of: host.number) ?? host.bounds) }
            // Activating an exposed sibling does not raise Rex through other apps.
            if let paneBody {
                raise = !Self.coveredByOtherApp(paneBody, above: windowsAbove(host.number), host: host.pid, sibling: pid)
            }
        }
        let level: NSWindow.Level = raise ? .floating : .normal
        guard window.level != level else { return }
        window.level = level
        if level == .normal, shown, let number = rex?.number {
            window.order(.above, relativeTo: Int(number))
        }
    }

    static func coveredByOtherApp(_ body: CGRect, above windows: [[String: Any]], host: pid_t, sibling: pid_t) -> Bool {
        let levelsLeafCanCover = NSWindow.Level.normal.rawValue...NSWindow.Level.floating.rawValue
        return windows.contains { item in
            guard let layer = item[kCGWindowLayer as String] as? Int, levelsLeafCanCover.contains(layer),
                  let pid = item[kCGWindowOwnerPID as String] as? pid_t, pid != host, pid != sibling, pid != getpid(),
                  let dictionary = item[kCGWindowBounds as String] as? NSDictionary,
                  let bounds = CGRect(dictionaryRepresentation: dictionary), bounds.intersects(body) else { return false }
            return NSRunningApplication(processIdentifier: pid)?.executableURL != Bundle.main.executableURL
        }
    }

    func applicationActivated(_ note: Notification) {
        guard let app = note.userInfo?[NSWorkspace.applicationUserInfoKey] as? NSRunningApplication else { return }
        updateLevel(for: app.processIdentifier, executable: app.executableURL)
        guard app.processIdentifier == rex?.pid, sessionIsShown else { return }
        // The mouse-down and Rex's new focused block can arrive after activation.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
            guard let self, self.sessionIsShown, self.rexIsActive else { return }
            self.requestFocusAfterRefresh()
        }
    }

    func reclaimFocusAfterActivation(at requestedAt: TimeInterval = ProcessInfo.processInfo.systemUptime) {
        focusRequestedByActivation = nil
        guard sessionIsShown, placement?.focused == true, !coveredByRex, rexIsActive,
              Date().timeIntervalSince(forwardedAt) > 1 else { return }
        guard shown else {
            // During layout animation lastFrame still belongs to the previous placement.
            if Date() < settleUntil { focusRequestedByActivation = requestedAt }
            return
        }
        if !clickedOutsidePane(at: requestedAt) { takeFocus() }
    }

    func clickedOutsidePane(at time: TimeInterval) -> Bool {
        guard let click = lastExternalClick, time - click.time <= 0.75 else { return false }
        // Both are AppKit screen points: extend upwards from the body to include its header.
        var pane = lastFrame
        pane.size.height += chrome.header
        return !pane.contains(click.point)
    }

    func takeFocus() {
        guard sessionIsShown else { return }
        guard shown else { focusWhenShown = true; return }
        guard placement?.focused == true, !coveredByRex, rexIsActive || NSApp.isActive else { return }
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
        "shift+cmd+c",
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
                    // A key mode's bindings apply only while Rex is in that mode,
                    // which this window can't see; outside it the key is the page's.
                    if let mode = item["mode"] as? String, !mode.isEmpty { continue }
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
        let inField = typingInField
        // Focus moved into a field after a multi-key binding began, such as a
        // click on the address bar: drop the binding so the key is text.
        if inField, pendingPrefix?.inField == false { pendingPrefix = nil }
        // A plain key typed into a field is text, even when Rex binds it.
        if pendingPrefix == nil, Chord.types(event), inField { return false }
        let typed = (pendingPrefix?.keys ?? []) + [chord]
        if let binding = bindings.first(where: { $0.keys == typed }) {
            pendingPrefix = nil
            perform(binding)
            return true
        }
        if bindings.contains(where: { $0.keys.count > typed.count && Array($0.keys.prefix(typed.count)) == typed }) {
            pendingPrefix = (typed, pendingPrefix?.inField ?? inField)
            return true
        }
        let swallowed = pendingPrefix != nil
        pendingPrefix = nil
        return swallowed
    }

    /// Whether the focus is somewhere text is typed: the address bar, find
    /// bar or composer (their field editor), or a field on the page.
    var typingInField: Bool {
        let responder = window.firstResponder
        if responder is NSText { return true }
        if let view = responder as? NSView, view === web || view.isDescendant(of: web) { return pageEditing }
        return false
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
            if event.window === self.browser.window { self.browser.lastInputAt = Date() }
            return self.browser.handleRexKey(event) ? nil : event
        }
        NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .leftMouseUp]) { [weak self] event in
            if let self, event.window === self.browser.window { self.browser.lastInputAt = Date() }
            return event
        }
        browser.start()
        let initial = ProcessInfo.processInfo.environment["LEAF_BROWSER_URL"] ?? ""
        // Wait for the first layout so the window appears in place, focused.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
            self.browser.takeFocus()
            if initial.isEmpty {
                self.browser.showHome()
                self.browser.focusAddress()
            } else {
                self.browser.window.makeFirstResponder(self.browser.web)
                self.browser.navigate(initial)
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) { browser.stopSidebarTracking() }

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
            item("Point at Element", #selector(Browser.togglePoint), "c", [.command, .shift], target: b),
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
