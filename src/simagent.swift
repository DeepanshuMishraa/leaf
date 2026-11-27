// What an agent drives a simulator with: the requests `leaf sim` sends, the
// screen's accessibility tree, and a headless mode that answers one request
// without a pane (`leaf-sim-host --drive`), for a simulator booted elsewhere.
//
// The tree comes from iOS's own accessibility runtime, the way Xcode's
// Accessibility Inspector reads it: AccessibilityPlatformTranslation
// (a private macOS framework) turns each element into a macOS accessibility
// element, and every attribute it asks for goes to the device through
// CoreSimulator's `sendAccessibilityRequestAsync`. No app needs changing.

import AppKit

// MARK: - Accessibility

/// The frontmost app's accessibility tree on one device.
enum Accessibility {
    private typealias SendFn = @convention(c) (AnyObject, Selector, AnyObject, DispatchQueue, @escaping @convention(block) (AnyObject?) -> Void) -> Void
    private typealias FrontmostFn = @convention(c) (AnyObject, Selector, UInt32, NSString) -> Unmanaged<AnyObject>?

    /// Answers the translator's requests by sending them to the device.
    /// Its methods are those of AXPTranslationTokenDelegateHelper, a
    /// private protocol, so the conformance is added at run time.
    final class Bridge: NSObject {
        let device: NSObject

        init(device: NSObject) { self.device = device }

        @objc func accessibilityTranslationDelegateBridgeCallbackWithToken(_ token: NSString) -> Any {
            let device = self.device
            let send: @convention(block) (AnyObject) -> AnyObject? = { request in
                Accessibility.send(request, to: device)
            }
            return send
        }

        /// Frames stay in the device's points.
        @objc func accessibilityTranslationConvertPlatformFrameToSystem(_ rect: CGRect, withToken token: NSString) -> CGRect { rect }

        @objc func accessibilityTranslationRootParentWithToken(_ token: NSString) -> Any? { nil }
    }

    /// When the walk under way must end; every request waits at most until then.
    private static var deadline = Date.distantFuture
    private static let deadlineLock = NSLock()

    private static var remaining: TimeInterval {
        deadlineLock.lock()
        defer { deadlineLock.unlock() }
        return deadline.timeIntervalSinceNow
    }

    /// One request, waited for; an empty response when the device doesn't
    /// answer in 3 s or by the walk's deadline.
    private static func send(_ request: AnyObject, to device: NSObject) -> AnyObject? {
        let selector = NSSelectorFromString("sendAccessibilityRequestAsync:completionQueue:completionHandler:")
        let empty = (NSClassFromString("AXPTranslatorResponse") as AnyObject?)?
            .perform(NSSelectorFromString("emptyResponse"))?.takeUnretainedValue()
        let wait = min(3, remaining)
        guard device.responds(to: selector), wait > 0 else { return empty }
        let done = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var response: AnyObject?
        unsafeBitCast(device.method(for: selector), to: SendFn.self)(device, selector, request, DispatchQueue.global(qos: .userInitiated)) { answer in
            lock.lock()
            response = answer
            lock.unlock()
            done.signal()
        }
        guard done.wait(timeout: .now() + wait) == .success else { return empty }
        lock.lock()
        defer { lock.unlock() }
        return response ?? empty
    }

    /// The shared translator, set up once for `device`.
    private static var translator: (translator: NSObject, bridge: Bridge)?
    private static let setup = NSLock()

    private static func translator(for device: NSObject) -> Result<NSObject, SimFailure> {
        setup.lock()
        defer { setup.unlock() }
        if let translator, translator.bridge.device === device { return .success(translator.translator) }
        let framework = "/System/Library/PrivateFrameworks/AccessibilityPlatformTranslation.framework/AccessibilityPlatformTranslation"
        guard dlopen(framework, RTLD_NOW) != nil,
              let type = NSClassFromString("AXPTranslator"),
              let shared = (type as AnyObject).perform(NSSelectorFromString("sharedInstance"))?.takeUnretainedValue() as? NSObject else {
            return .failure(SimFailure(message: "This Mac doesn't have AccessibilityPlatformTranslation's AXPTranslator, which leaf reads the simulator's screen with. Use `leaf sim screenshot` instead, and please report your macOS version."))
        }
        guard device.responds(to: NSSelectorFromString("sendAccessibilityRequestAsync:completionQueue:completionHandler:")) else {
            return .failure(SimFailure(message: CoreSim.unsupported("sendAccessibilityRequestAsync:completionQueue:completionHandler:")))
        }
        if let helper = objc_getProtocol("AXPTranslationTokenDelegateHelper") { class_addProtocol(Bridge.self, helper) }
        let bridge = Bridge(device: device)
        shared.setValue(true, forKey: "supportsDelegateTokens")
        shared.setValue(true, forKey: "accessibilityEnabled")
        shared.setValue(bridge, forKey: "bridgeTokenDelegate")
        translator = (shared, bridge)
        return .success(shared)
    }

    /// One element, in the device's points from the top left of the screen.
    struct Node: Equatable {
        let depth: Int
        let role: String
        let label: String
        let value: String
        let identifier: String
        let frame: CGRect
        let enabled: Bool
    }

    /// The frontmost app's elements, depth first, the app itself first.
    static func tree(of device: NSObject, until end: Date) -> Result<[Node], SimFailure> {
        deadlineLock.lock()
        deadline = end
        deadlineLock.unlock()
        defer {
            deadlineLock.lock()
            deadline = .distantFuture
            deadlineLock.unlock()
        }
        let translator: NSObject
        switch self.translator(for: device) {
        case .success(let found): translator = found
        case .failure(let failure): return .failure(failure)
        }
        let token = UUID().uuidString as NSString
        let frontmost = NSSelectorFromString("frontmostApplicationWithDisplayId:bridgeDelegateToken:")
        let toElement = NSSelectorFromString("macPlatformElementFromTranslation:")
        guard translator.responds(to: frontmost), translator.responds(to: toElement) else {
            return .failure(SimFailure(message: CoreSim.unsupported("AXPTranslator's frontmost application")))
        }
        guard let translation = unsafeBitCast(translator.method(for: frontmost), to: FrontmostFn.self)(translator, frontmost, 0, token)?
                .takeUnretainedValue() as? NSObject else {
            return .failure(SimFailure(message: "The simulator didn't say which app is in front, so leaf can't read its screen. It may still be starting or be locked; take a screenshot with `leaf sim screenshot` to see."))
        }
        translation.setValue(token, forKey: "bridgeDelegateToken")
        guard let root = translator.perform(toElement, with: translation)?.takeUnretainedValue() as? NSAccessibilityElement else {
            return .failure(SimFailure(message: CoreSim.unsupported("AXPTranslator's platform elements")))
        }
        var nodes: [Node] = []
        func text(_ value: Any?) -> String {
            switch value {
            case let string as String: return string
            case let attributed as NSAttributedString: return attributed.string
            case let number as NSNumber: return number.stringValue
            default: return ""
            }
        }
        func walk(_ element: NSAccessibilityElement, depth: Int) {
            // Children come back without the token; requests need it.
            (element.value(forKey: "translation") as? NSObject)?.setValue(token, forKey: "bridgeDelegateToken")
            let role = element.accessibilityRole()?.rawValue ?? ""
            nodes.append(Node(depth: depth,
                              role: role.hasPrefix("AX") ? String(role.dropFirst(2)) : role,
                              label: text(element.accessibilityLabel()),
                              value: text(element.accessibilityValue()),
                              identifier: element.accessibilityIdentifier() ?? "",
                              frame: element.accessibilityFrame(),
                              enabled: element.isAccessibilityEnabled()))
            // Deeper or bigger than any real screen: a guard against cycles.
            guard depth < 64 else { return }
            for child in element.accessibilityChildren() ?? [] {
                guard nodes.count < 2000, remaining > 0 else { return }
                if let child = child as? NSAccessibilityElement { walk(child, depth: depth + 1) }
            }
        }
        walk(root, depth: 0)
        // Past the deadline the walk stopped part way, and half a tree
        // would read as a screen with things missing.
        guard remaining > 0 else {
            return .failure(SimFailure(message: "The simulator stopped answering accessibility requests while leaf read its screen. It may be busy or hung; take a screenshot with `leaf sim screenshot` to see, then try again."))
        }
        return .success(nodes)
    }

    /// The tree once it has stayed the same for 0.3 s (an animation or a
    /// load has finished; a push can hold still for 0.1 s mid-slide), or the
    /// latest after `limit` seconds.
    /// Each reading may take up to 3 s past `limit`, as a slow device
    /// needs; the last complete one stands if a later one runs out of time.
    static func settledTree(of device: NSObject, limit: TimeInterval) -> Result<[Node], SimFailure> {
        let deadline = Date().addingTimeInterval(limit)
        var last: [Node]?
        var since = Date()
        while true {
            switch tree(of: device, until: max(deadline, Date()).addingTimeInterval(3)) {
            case .failure(let failure):
                if let last { return .success(last) }
                return .failure(failure)
            case .success(let nodes):
                let now = Date()
                if nodes != last { (last, since) = (nodes, now) }
                if now.timeIntervalSince(since) >= 0.3 || now >= deadline { return .success(nodes) }
            }
            usleep(100_000)
        }
    }
}

// MARK: - Requests

/// A screen ready for input.
struct LiveScreen {
    let hid: HID
    let device: NSObject
    /// The screen's size in screenshot pixels.
    let width: Int
    let height: Int
    /// The open Duo's inner screen: its screenshot is landscape, the
    /// framebuffer under it portrait.
    let turned: Bool
    /// What `info` reports: the device and, on the Duo, the screen in use.
    let info: [String: Any]
}

/// One request from `leaf sim`, from the pane's socket or headless. Coordinates are
/// screenshot pixels, as `leaf sim screenshot` saves them.
enum Driver {
    /// Modifier keys by the names `leaf sim key cmd+a` takes.
    static let modifiers: [String: UInt32] = [
        "cmd": 0xe3, "command": 0xe3, "shift": 0xe1, "ctrl": 0xe0, "control": 0xe0,
        "alt": 0xe2, "option": 0xe2, "opt": 0xe2,
    ]

    static func failed(_ message: String) -> [String: Any] { ["failed": ["message": message]] }

    /// `ripple` shows where a touch lands, as fractions of the framebuffer.
    static func handle(_ request: [String: Any], on screen: LiveScreen, ripple: (CGPoint) -> Void) -> [String: Any] {
        let (hid, width, height) = (screen.hid, screen.width, screen.height)
        func fraction(_ value: Any?, _ size: Int) -> CGFloat? {
            guard let number = value as? NSNumber else { return nil }
            return CGFloat(truncating: number) / CGFloat(size)
        }
        /// A screenshot pixel as a fraction of the framebuffer.
        func point(_ value: Any?) -> CGPoint? {
            guard let pair = value as? [Any], pair.count == 2,
                  let x = fraction(pair[0], width), let y = fraction(pair[1], height),
                  (0...1).contains(x), (0...1).contains(y) else { return nil }
            return screen.turned ? CGPoint(x: y, y: 1 - x) : CGPoint(x: x, y: y)
        }
        let outside = "is outside the screen, which is \(width)×\(height) pixels with 0,0 at the top left"
        /// False when the request was worked out for a screen of another
        /// size, as when the Duo folds between reading an element and touching it.
        func sameScreen(_ action: [String: Any]) -> Bool {
            guard let size = action["size"] as? [NSNumber], size.count == 2 else { return true }
            return size[0].intValue == width && size[1].intValue == height
        }
        let resized = "The screen changed size (it's \(width)×\(height) pixels now) since leaf read the element, so nothing was touched. Run `leaf sim snapshot` and try again."
        if let tap = request["tap"] as? [String: Any] {
            guard sameScreen(tap) else { return failed(resized) }
            guard let at = point(tap["at"]) else { return failed("The tap \(outside).") }
            ripple(at)
            hid.tap(at: at)
            return ["ok": [:]]
        }
        if let swipe = request["swipe"] as? [String: Any] {
            guard sameScreen(swipe) else { return failed(resized) }
            guard let from = point(swipe["from"]), let to = point(swipe["to"]) else {
                return failed("The swipe \(outside).")
            }
            let duration = min(max((swipe["duration"] as? NSNumber)?.doubleValue ?? 0.3, 0.05), 5)
            ripple(from)
            hid.swipe(from: from, to: to, duration: duration)
            return ["ok": [:]]
        }
        if let type = request["type"] as? [String: Any], let text = type["text"] as? String {
            let skipped = hid.type(text)
            if skipped.isEmpty { return ["ok": [:]] }
            let list = String(Set(skipped).prefix(20))
            let udid = screen.info["udid"] as? String ?? "<udid>"
            return failed("Typed the rest, but these characters aren't on a US keyboard and were skipped: \(list). Paste such text with `xcrun simctl pbcopy \(udid)` and a long-press Paste.")
        }
        if let key = request["key"] as? [String: Any], let name = key["name"] as? String {
            return press(key: name, hid: hid)
        }
        if let button = request["button"] as? [String: Any], let name = button["name"] as? String {
            guard let which = HID.Button(rawValue: name.lowercased()) else {
                return failed("No button named \(name). Buttons: \(HID.Button.allCases.map(\.rawValue).joined(separator: ", ")).")
            }
            hid.press(which)
            return ["ok": [:]]
        }
        if let snapshot = request["snapshot"] as? [String: Any] {
            let settle = (snapshot["settle"] as? NSNumber)?.doubleValue ?? 2
            return self.snapshot(screen, settle: min(max(settle, 0), 10))
        }
        if request["info"] != nil {
            var info = screen.info
            info["width"] = width
            info["height"] = height
            return ["ok": info]
        }
        return failed("leaf's simulator pane didn't understand that request. Update leaf so the CLI and the pane match.")
    }

    /// A key or a chord such as `cmd+a`, pressed and let go.
    static func press(key name: String, hid: HID) -> [String: Any] {
        // Empty parts are kept, so `a+` has no key; two empty parts at the
        // end are the + key itself, as in `+` or `cmd++`.
        var parts = name.split(separator: "+", omittingEmptySubsequences: false).map(String.init)
        if parts.count >= 2, parts[parts.count - 1].isEmpty, parts[parts.count - 2].isEmpty {
            parts.removeLast(2)
            parts.append("+")
        }
        // The key keeps its case: `A` is shift+a.
        let last = parts.popLast() ?? ""
        guard !last.isEmpty else {
            return failed("\(name) has no key after its last +. Write chords like cmd+a.")
        }
        let held = parts.compactMap { modifiers[$0.lowercased()] }
        guard held.count == parts.count else {
            return failed("No modifier key in \(name). Modifiers: cmd, shift, ctrl, alt.")
        }
        var shifted = false
        let usage: UInt32
        if let named = Keys.named[last.lowercased()] {
            usage = named
        } else if last.count == 1, let character = last.first, let stroke = Keys.stroke(for: character) {
            (usage, shifted) = (stroke.usage, stroke.shift)
        } else {
            return failed("No key named \(name). Keys: \(Keys.named.keys.sorted().joined(separator: ", ")), or one character, after cmd+, shift+, ctrl+ or alt+.")
        }
        let down = held + (shifted && !held.contains(Keys.leftShift) ? [Keys.leftShift] : [])
        for modifier in down { hid.key(modifier, down: true) }
        hid.key(usage, down: true)
        usleep(40_000)
        hid.key(usage, down: false)
        for modifier in down.reversed() { hid.key(modifier, down: false) }
        hid.queue.sync {}
        return ["ok": [:]]
    }

    /// The frontmost app's elements with frames in screenshot pixels.
    static func snapshot(_ screen: LiveScreen, settle: TimeInterval) -> [String: Any] {
        let nodes: [Accessibility.Node]
        switch Accessibility.settledTree(of: screen.device, limit: settle) {
        case .success(let found): nodes = found
        case .failure(let failure): return failed(failure.message)
        }
        // The app's own frame is the whole screen, in points.
        let points = nodes.first.map(\.frame.size) ?? .zero
        let scaleX = points.width > 0 ? CGFloat(screen.width) / points.width : 1
        let scaleY = points.height > 0 ? CGFloat(screen.height) / points.height : 1
        let elements: [[String: Any]] = nodes.map { node in
            let frame = node.frame
            var element: [String: Any] = [
                "depth": node.depth,
                "role": node.role,
                "frame": [Int((frame.minX * scaleX).rounded()), Int((frame.minY * scaleY).rounded()),
                          Int((frame.width * scaleX).rounded()), Int((frame.height * scaleY).rounded())],
            ]
            if !node.label.isEmpty { element["label"] = node.label }
            if !node.value.isEmpty { element["value"] = node.value }
            if !node.identifier.isEmpty { element["id"] = node.identifier }
            if !node.enabled { element["disabled"] = true }
            return element
        }
        return ["ok": ["width": screen.width, "height": screen.height, "elements": elements]]
    }
}

// MARK: - Headless

/// `leaf-sim-host --drive`: one request on stdin, its answer on stdout, for
/// a booted simulator that isn't open in a Leaf pane (Simulator.app's, or
/// one booted with simctl). Touches land the same way; nobody sees a ring.
enum Headless {
    static func run() -> Never {
        let reply = answer(FileHandle.standardInput.readDataToEndOfFile())
        var data = (try? JSONSerialization.data(withJSONObject: reply)) ?? Data(#"{"failed":{"message":"leaf couldn't write the simulator's answer."}}"#.utf8)
        data.append(UInt8(ascii: "\n"))
        FileHandle.standardOutput.write(data)
        exit(0)
    }

    static func answer(_ input: Data) -> [String: Any] {
        let environment = ProcessInfo.processInfo.environment
        guard let udid = environment["LEAF_SIM_UDID"], !udid.isEmpty,
              let developerDir = environment["LEAF_DEVELOPER_DIR"], !developerDir.isEmpty else {
            return Driver.failed("leaf-sim-host --drive needs LEAF_SIM_UDID and LEAF_DEVELOPER_DIR; run it through `leaf sim`.")
        }
        guard let request = (try? JSONSerialization.jsonObject(with: input)) as? [String: Any] else {
            return Driver.failed("leaf's simulator driver got a request it couldn't read.")
        }
        let set: NSObject
        switch CoreSim.deviceSet(developerDir: developerDir) {
        case .success(let found): set = found
        case .failure(let failure): return Driver.failed(failure.message)
        }
        guard let device = CoreSim.device(udid, in: set) else {
            return Driver.failed("CoreSimulator has no device \(udid). `leaf sim list` shows the ids.")
        }
        // The smallest built-in screen: the only one, or the Duo's cover.
        let displays = CoreSim.builtInDisplays(of: device)
        let surfaces = displays.compactMap(CoreSim.surface(of:))
        guard let surface = surfaces.min(by: { $0.width * $0.height < $1.width * $1.height }) else {
            return Driver.failed("Simulator \(udid) has no screen: it isn't booted, or is still starting. Boot it with `xcrun simctl boot \(udid)` (or open it in leaf) and wait for `xcrun simctl bootstatus \(udid)`, then try again.")
        }
        // The Duo's touches need a digitizer for the screen in front, which
        // only the pane sets up (it knows which screen that is). Without
        // one, backboardd drops them or worse, so headless refuses them,
        // whether or not the second screen has drawn yet.
        if displays.count > 1, request["tap"] != nil || request["swipe"] != nil {
            return Driver.failed("Simulator \(udid) has two screens (an iPhone Duo), and leaf can only touch one from its pane. Open it with `leaf sim open \(udid)` in a Rex pane, then try again. Snapshots, typing, keys and buttons work without one.")
        }
        let hid: HID
        switch HID.connect(to: device) {
        case .success(let connected): hid = connected
        case .failure(let failure): return Driver.failed(failure.message)
        }
        let name = environment["LEAF_SIM_NAME"] ?? ""
        let runtime = environment["LEAF_SIM_RUNTIME"] ?? ""
        let screen = LiveScreen(hid: hid, device: device, width: surface.width, height: surface.height, turned: false,
                                info: ["udid": udid, "name": name, "runtime": runtime])
        let reply = Driver.handle(request, on: screen) { _ in }
        // Let the last HID message leave before the process does.
        hid.queue.sync {}
        usleep(30_000)
        return reply
    }
}
