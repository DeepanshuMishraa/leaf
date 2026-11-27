// Leaf's iOS Simulator pane: a simulator's live screen on a 3D iPhone, in a
// window kept over one Rex block (see src/pane.swift). The iPhone Duo
// (LEAF_SIM_DUO=1) is drawn on a foldable model that swings open and shut.
// A helper inside the device (src/duo-hinge.m) turns iOS's hinge with it, the
// way Xcode's Device Hub does, so iOS moves between the cover screen (folded)
// and the inner one (open); the phone shows whichever faces front, and touches
// go to a digitizer leaf registers for that screen.
//
// The screen is the simulator's own framebuffer: CoreSimulator hands out the
// IOSurface it draws into, and a Metal texture over that surface is the
// phone's screen material, so no frame is copied, encoded or sent anywhere.
// Touches and keys go back as Indigo HID messages through SimulatorKit, the
// way Xcode's Simulator app sends them. Both frameworks are private and are
// opened at run time from the selected Xcode; leaf boots the device with
// `simctl`.
//
// An agent drives the same device through a Unix socket (LEAF_SIM_SOCKET):
// `leaf sim tap|swipe|type|key|button` send one JSON line and read one back.
//
// Environment: REX_SESSION, REX_BLOCK, LEAF_REX (see pane.swift);
// LEAF_SIM_UDID, LEAF_SIM_NAME, LEAF_SIM_RUNTIME (the device), LEAF_SIM_SOCKET,
// LEAF_DEVELOPER_DIR (the Xcode developer folder), LEAF_AGENT.

import AppKit
import Metal
import SceneKit

// MARK: - Errors

/// Why the simulator can't be shown or driven; `message` is for the person.
struct SimFailure: Error {
    let message: String
}

// MARK: - CoreSimulator

/// The private CoreSimulator and SimulatorKit calls this pane needs, looked
/// up by name so a missing one fails with a message instead of a crash.
enum CoreSim {
    private typealias ContextFn = @convention(c) (AnyObject, Selector, NSString, UnsafeMutableRawPointer?) -> Unmanaged<AnyObject>?
    private typealias DeviceSetFn = @convention(c) (AnyObject, Selector, UnsafeMutableRawPointer?) -> Unmanaged<AnyObject>?

    /// Opens both frameworks from `developerDir` and returns the device set.
    static func deviceSet(developerDir: String) -> Result<NSObject, SimFailure> {
        let xcode = URL(fileURLWithPath: developerDir).deletingLastPathComponent().path
        let coreSimulator = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator"
        guard dlopen(coreSimulator, RTLD_NOW) != nil else {
            return .failure(SimFailure(message: "CoreSimulator isn't installed (\(coreSimulator) is missing). Open Xcode once so it can finish installing its components, then try again."))
        }
        let kits = ["\(xcode)/SharedFrameworks/SimulatorKit.framework/SimulatorKit",
                    "\(developerDir)/Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit"]
        guard kits.contains(where: { dlopen($0, RTLD_NOW) != nil }) else {
            return .failure(SimFailure(message: "SimulatorKit wasn't found in \(xcode). Check that `xcode-select -p` points at a full Xcode, not the Command Line Tools."))
        }
        guard let contextClass = NSClassFromString("SimServiceContext") else {
            return .failure(SimFailure(message: unsupported("SimServiceContext")))
        }
        let contextSelector = NSSelectorFromString("sharedServiceContextForDeveloperDir:error:")
        guard let contextMethod = class_getClassMethod(contextClass, contextSelector) else {
            return .failure(SimFailure(message: unsupported("sharedServiceContextForDeveloperDir:error:")))
        }
        var error: Unmanaged<NSError>?
        let context = withUnsafeMutablePointer(to: &error) { pointer in
            unsafeBitCast(method_getImplementation(contextMethod), to: ContextFn.self)(
                contextClass, contextSelector, developerDir as NSString, UnsafeMutableRawPointer(pointer))
        }
        guard let context = context?.takeUnretainedValue() as? NSObject else {
            let why = error?.takeUnretainedValue().localizedDescription ?? "no reason given"
            return .failure(SimFailure(message: "CoreSimulator wouldn't start (\(why)). Run `xcrun simctl list` in a terminal to see whether the simulator service works."))
        }
        let setSelector = NSSelectorFromString("defaultDeviceSetWithError:")
        guard context.responds(to: setSelector) else {
            return .failure(SimFailure(message: unsupported("defaultDeviceSetWithError:")))
        }
        error = nil
        let set = withUnsafeMutablePointer(to: &error) { pointer in
            unsafeBitCast(context.method(for: setSelector), to: DeviceSetFn.self)(
                context, setSelector, UnsafeMutableRawPointer(pointer))
        }
        guard let set = set?.takeUnretainedValue() as? NSObject else {
            let why = error?.takeUnretainedValue().localizedDescription ?? "no reason given"
            return .failure(SimFailure(message: "CoreSimulator couldn't open its device set (\(why))."))
        }
        return .success(set)
    }

    /// The device with `udid` in `set`.
    static func device(_ udid: String, in set: NSObject) -> NSObject? {
        guard set.responds(to: NSSelectorFromString("devices")),
              let devices = set.value(forKey: "devices") as? [NSObject] else { return nil }
        return devices.first { device in
            guard device.responds(to: NSSelectorFromString("UDID")) else { return false }
            return (device.value(forKey: "UDID") as? UUID)?.uuidString.caseInsensitiveCompare(udid) == .orderedSame
        }
    }

    /// The object-valued property `key` of `object`, read by sending the
    /// message. Newer CoreSimulator hands out remote proxies (ROCKRemoteProxy)
    /// for device ports: they forward messages, but KVC's `value(forKey:)`
    /// looks the key up on the proxy's own class and throws.
    static func object(_ object: NSObject, _ key: String) -> AnyObject? {
        let selector = NSSelectorFromString(key)
        guard object.responds(to: selector) else { return nil }
        return object.perform(selector)?.takeUnretainedValue()
    }

    private typealias UInt16Fn = @convention(c) (AnyObject, Selector) -> UInt16

    /// The built-in screens' port descriptors: those that render to an
    /// IOSurface with display class 0, or any that render when none says so.
    /// The iPhone Duo has two, its cover and its inner screen.
    static func builtInDisplays(of device: NSObject) -> [NSObject] {
        guard let io = object(device, "io") as? NSObject,
              let ports = object(io, "ioPorts") as? [NSObject] else { return [] }
        var found: [NSObject] = []
        for port in ports {
            guard let descriptor = object(port, "descriptor") as? NSObject,
                  descriptor.responds(to: NSSelectorFromString("framebufferSurface"))
                    || descriptor.responds(to: NSSelectorFromString("ioSurface")) else { continue }
            found.append(descriptor)
        }
        let displayClass = NSSelectorFromString("displayClass")
        let builtIn = found.filter { descriptor in
            guard let state = object(descriptor, "state") as? NSObject,
                  state.responds(to: displayClass) else { return false }
            // displayClass is a small integer, not an object, so it can't go
            // through perform; the low 16 bits hold it whatever its width.
            return unsafeBitCast(state.method(for: displayClass), to: UInt16Fn.self)(state, displayClass) == 0
        }
        return builtIn.isEmpty ? Array(found.prefix(1)) : builtIn
    }

    /// The surface `descriptor` draws the screen into right now.
    static func surface(of descriptor: NSObject) -> IOSurface? {
        for key in ["framebufferSurface", "ioSurface"] {
            if let surface = object(descriptor, key) as? IOSurface { return surface }
        }
        return nil
    }

    private typealias SurfaceCallbackFn = @convention(c) (AnyObject, Selector, NSUUID, @convention(block) (IOSurface?, IOSurface?) -> Void) -> Void

    /// Calls `changed` (on any queue) when the display swaps its surface, as
    /// it does when the screen's size changes. False when it can't be watched.
    static func watchSurfaces(of descriptor: NSObject, _ changed: @escaping () -> Void) -> Bool {
        let selector = NSSelectorFromString("registerCallbackWithUUID:ioSurfacesChangeCallback:")
        guard descriptor.responds(to: selector) else { return false }
        let block: @convention(block) (IOSurface?, IOSurface?) -> Void = { _, _ in changed() }
        unsafeBitCast(descriptor.method(for: selector), to: SurfaceCallbackFn.self)(descriptor, selector, NSUUID(), block)
        return true
    }

    static func unsupported(_ name: String) -> String {
        "This Xcode's simulator frameworks don't have \(name), which leaf's simulator pane needs. The Xcode at `xcode-select -p` may be newer or older than leaf supports; please report it with the Xcode version from `xcodebuild -version`."
    }
}

// MARK: - HID

/// Touches, hardware buttons and keys for one device, sent the way
/// Simulator.app sends them. Thread-safe: every send goes through `queue`.
final class HID {
    private typealias InitFn = @convention(c) (AnyObject, Selector, AnyObject, UnsafeMutableRawPointer?) -> Unmanaged<AnyObject>?
    private typealias SendFn = @convention(c) (AnyObject, Selector, UnsafeMutableRawPointer, ObjCBool, AnyObject?, AnyObject?) -> Void
    private typealias MouseFn = @convention(c) (UnsafePointer<CGPoint>, UnsafePointer<CGPoint>?, UInt32, UInt, CGSize, UInt) -> UnsafeMutableRawPointer?
    private typealias ButtonFn = @convention(c) (UInt32, UInt32, UInt32) -> UnsafeMutableRawPointer?
    private typealias KeyFn = @convention(c) (UInt32, UInt32) -> UnsafeMutableRawPointer?

    enum Phase { case down, moved, up }

    enum Button: String, CaseIterable {
        case home, lock, side, siri

        /// Indigo's event source for the button.
        var source: UInt32 {
            switch self {
            case .home: return 0x0
            case .lock: return 0x1
            case .side: return 0xbb8
            case .siri: return 0x400002
            }
        }
    }

    private let client: AnyObject
    private let sendFn: SendFn
    private let sendSelector: Selector
    private let mouse: MouseFn
    private let button: ButtonFn
    private let keyboard: KeyFn
    let queue = DispatchQueue(label: "leaf.sim.hid")
    /// Set from the send callback when the device refuses a message.
    private(set) var lastError: String?

    /// Indigo's target for the built-in touch screen and for hardware buttons.
    private static let touchTarget: UInt32 = 0x32
    private static let buttonTarget: UInt32 = 0x33
    /// Set on a target to name a screen by its ID instead.
    private static let screenTargetBit: UInt32 = 0x4000_0000

    /// Where touches land.
    enum TouchScreen {
        /// The built-in digitizer: the screen on every iPhone but the Duo.
        case builtIn
        /// A digitizer leaf registers for one screen, by its simctl screen
        /// ID and unique ID. The Duo's built-in digitizer is bound to its
        /// inner screen, which iOS leaves dark; its iOS runs on the cover.
        case screen(id: UInt32, uniqueID: String)
    }

    /// Read and written on `queue`.
    private var screen = TouchScreen.builtIn

    private init(client: AnyObject, sendFn: SendFn, sendSelector: Selector, mouse: MouseFn, button: ButtonFn, keyboard: KeyFn) {
        self.client = client
        self.sendFn = sendFn
        self.sendSelector = sendSelector
        self.mouse = mouse
        self.button = button
        self.keyboard = keyboard
    }

    static func connect(to device: NSObject) -> Result<HID, SimFailure> {
        func symbol<T>(_ name: String, as type: T.Type) -> T? {
            guard let pointer = dlsym(UnsafeMutableRawPointer(bitPattern: -2), name) else { return nil } // RTLD_DEFAULT
            return unsafeBitCast(pointer, to: type)
        }
        guard let mouse = symbol("IndigoHIDMessageForMouseNSEvent", as: MouseFn.self),
              let button = symbol("IndigoHIDMessageForButton", as: ButtonFn.self),
              let key = symbol("IndigoHIDMessageForKeyboardArbitrary", as: KeyFn.self) else {
            return .failure(SimFailure(message: CoreSim.unsupported("the Indigo HID message functions")))
        }
        guard let type = NSClassFromString("SimulatorKit.SimDeviceLegacyHIDClient")
                ?? NSClassFromString("_TtC12SimulatorKit24SimDeviceLegacyHIDClient") else {
            return .failure(SimFailure(message: CoreSim.unsupported("SimDeviceLegacyHIDClient")))
        }
        let initSelector = NSSelectorFromString("initWithDevice:error:")
        let sendSelector = NSSelectorFromString("sendWithMessage:freeWhenDone:completionQueue:completion:")
        guard let initMethod = class_getInstanceMethod(type, initSelector),
              let sendMethod = class_getInstanceMethod(type, sendSelector),
              let allocated = (type as AnyObject).perform(NSSelectorFromString("alloc"))?.takeUnretainedValue() else {
            return .failure(SimFailure(message: CoreSim.unsupported("the HID client's init and send methods")))
        }
        var error: Unmanaged<NSError>?
        // `init` takes ownership of what `alloc` returned and returns +1.
        let client = withUnsafeMutablePointer(to: &error) { pointer in
            unsafeBitCast(method_getImplementation(initMethod), to: InitFn.self)(
                allocated, initSelector, device, UnsafeMutableRawPointer(pointer))
        }
        guard let client = client?.takeRetainedValue() else {
            let why = error?.takeUnretainedValue().localizedDescription ?? "no reason given"
            return .failure(SimFailure(message: "The simulator wouldn't accept input (\(why)). The screen still shows; close and reopen the pane to try again."))
        }
        return .success(HID(client: client, sendFn: unsafeBitCast(method_getImplementation(sendMethod), to: SendFn.self),
                            sendSelector: sendSelector, mouse: mouse, button: button, keyboard: key))
    }

    /// Sends a message the Indigo functions allocated; the client frees it.
    private func send(_ message: UnsafeMutableRawPointer?) {
        guard let message else { return }
        let done: @convention(block) (NSError?) -> Void = { [weak self] error in
            if let error { self?.lastError = error.localizedDescription }
        }
        sendFn(client, sendSelector, message, true, queue, done as AnyObject)
    }

    /// Sends later touches to `screen`.
    func route(to screen: TouchScreen) {
        queue.async {
            self.screen = screen
            self.registerDigitizer()
        }
    }

    /// Indigo's digitizer service commands.
    private enum DigitizerCommand: UInt32 {
        case create = 1, remove = 2
    }

    /// Gives the routed screen a fresh digitizer, as Xcode's display view
    /// does for screens without the built-in one. backboardd aborts on a
    /// touch for a screen with no digitizer, or for one whose digitizer was
    /// disconnected (as it is when that panel powers off, say when the Duo
    /// folds shut and locks), and a backboardd restart forgets them all. So
    /// before every touch the old one is removed and a new one made.
    private func registerDigitizer() {
        guard case .screen(let id, let uniqueID) = screen else { return }
        send(digitizerCommand(.remove, screenID: id, uniqueID: nil))
        send(digitizerCommand(.create, screenID: id, uniqueID: uniqueID))
    }

    /// Indigo's service command: one 0xa0-byte payload, the command, the
    /// target, built in, then the screen's unique ID as a C string of at
    /// most 63 bytes.
    private func digitizerCommand(_ command: DigitizerCommand, screenID: UInt32, uniqueID: String?) -> UnsafeMutableRawPointer? {
        guard let message = calloc(1, 0xc0) else { return nil }
        message.storeBytes(of: UInt32(0xa0), toByteOffset: 0x18, as: UInt32.self)
        message.storeBytes(of: UInt8(1), toByteOffset: 0x1c, as: UInt8.self)
        message.storeBytes(of: UInt32(0x7fff_0001), toByteOffset: 0x20, as: UInt32.self)
        message.storeBytes(of: command.rawValue, toByteOffset: 0x30, as: UInt32.self)
        message.storeBytes(of: HID.screenTargetBit | screenID, toByteOffset: 0x40, as: UInt32.self)
        message.storeBytes(of: UInt8(1), toByteOffset: 0x44, as: UInt8.self)
        for (offset, byte) in (uniqueID ?? "").utf8.prefix(63).enumerated() {
            message.storeBytes(of: byte, toByteOffset: 0x45 + offset, as: UInt8.self)
        }
        return message
    }

    private var touchTarget: UInt32 {
        switch screen {
        case .builtIn: return HID.touchTarget
        case .screen(let id, _): return HID.screenTargetBit | id
        }
    }

    /// One touch event at `point`, as fractions of the screen from its top left.
    func touch(_ phase: Phase, at point: CGPoint) {
        queue.async { self.touchNow(phase, at: point) }
    }

    private func touchNow(_ phase: Phase, at point: CGPoint) {
        var location = CGPoint(x: min(max(point.x, 0), 1), y: min(max(point.y, 0), 1))
        let type: NSEvent.EventType
        switch phase {
        case .down:
            registerDigitizer()
            type = .leftMouseDown
        case .moved: type = .leftMouseDragged
        case .up: type = .leftMouseUp
        }
        // A move within 16 ms of the last comes back nil: Indigo drops it.
        send(mouse(&location, nil, touchTarget, UInt(type.rawValue), CGSize(width: 1, height: 1), 0))
    }

    /// A tap at `point` (fractions of the screen); returns once it's sent.
    func tap(at point: CGPoint) {
        queue.sync {
            touchNow(.down, at: point)
            usleep(60_000)
            touchNow(.up, at: point)
        }
    }

    /// A straight drag from `start` to `end` over `duration` seconds.
    func swipe(from start: CGPoint, to end: CGPoint, duration: TimeInterval) {
        queue.sync {
            let steps = max(2, Int(duration / 0.017))
            touchNow(.down, at: start)
            for step in 1...steps {
                usleep(17_000)
                let t = CGFloat(step) / CGFloat(steps)
                touchNow(.moved, at: CGPoint(x: start.x + (end.x - start.x) * t, y: start.y + (end.y - start.y) * t))
            }
            touchNow(.up, at: end)
        }
    }

    func press(_ which: Button) {
        queue.sync {
            send(button(which.source, 1, HID.buttonTarget))
            usleep(80_000)
            send(button(which.source, 2, HID.buttonTarget))
        }
    }

    /// A key going down or up, by USB HID usage (page 7).
    func key(_ usage: UInt32, down: Bool) {
        queue.async { self.send(self.keyboard(usage, down ? 1 : 2)) }
    }

    /// Types `text` key by key on a US layout; returns the characters it
    /// couldn't type, which are skipped.
    @discardableResult
    func type(_ text: String) -> [Character] {
        var skipped: [Character] = []
        queue.sync {
            for character in text {
                guard let stroke = Keys.stroke(for: character) else { skipped.append(character); continue }
                if stroke.shift { send(keyboard(Keys.leftShift, 1)) }
                send(keyboard(stroke.usage, 1))
                usleep(12_000)
                send(keyboard(stroke.usage, 2))
                if stroke.shift { send(keyboard(Keys.leftShift, 2)) }
                usleep(12_000)
            }
        }
        return skipped
    }
}

/// USB HID keyboard usages (page 7) for a US layout.
enum Keys {
    static let leftShift: UInt32 = 0xe1

    static let named: [String: UInt32] = [
        "enter": 0x28, "return": 0x28, "escape": 0x29, "esc": 0x29, "backspace": 0x2a, "delete": 0x2a,
        "tab": 0x2b, "space": 0x2c, "right": 0x4f, "left": 0x50, "down": 0x51, "up": 0x52,
        "forwarddelete": 0x4c, "home": 0x4a, "end": 0x4d, "pageup": 0x4b, "pagedown": 0x4e,
    ]

    private static let unshifted: [Character: UInt32] = {
        var map: [Character: UInt32] = [:]
        for (index, letter) in "abcdefghijklmnopqrstuvwxyz".enumerated() { map[letter] = 0x04 + UInt32(index) }
        for (index, digit) in "1234567890".enumerated() { map[digit] = 0x1e + UInt32(index) }
        let rest: [Character: UInt32] = [
            "\n": 0x28, "\t": 0x2b, " ": 0x2c, "-": 0x2d, "=": 0x2e, "[": 0x2f, "]": 0x30, "\\": 0x31,
            ";": 0x33, "'": 0x34, "`": 0x35, ",": 0x36, ".": 0x37, "/": 0x38,
        ]
        map.merge(rest) { first, _ in first }
        return map
    }()

    private static let shifted: [Character: Character] = [
        "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8", "(": "9", ")": "0",
        "_": "-", "+": "=", "{": "[", "}": "]", "|": "\\", ":": ";", "\"": "'", "~": "`", "<": ",", ">": ".",
        "?": "/",
    ]

    static func stroke(for character: Character) -> (usage: UInt32, shift: Bool)? {
        if let usage = unshifted[character] { return (usage, false) }
        if let base = shifted[character], let usage = unshifted[base] { return (usage, true) }
        if character.isUppercase, let lower = character.lowercased().first, let usage = unshifted[lower] {
            return (usage, true)
        }
        return nil
    }

    /// macOS virtual key codes (ANSI) to HID usages, for typing in the pane.
    static let fromKeyCode: [UInt16: UInt32] = [
        0x00: 0x04, 0x0b: 0x05, 0x08: 0x06, 0x02: 0x07, 0x0e: 0x08, 0x03: 0x09, 0x05: 0x0a, 0x04: 0x0b,
        0x22: 0x0c, 0x26: 0x0d, 0x28: 0x0e, 0x25: 0x0f, 0x2e: 0x10, 0x2d: 0x11, 0x1f: 0x12, 0x23: 0x13,
        0x0c: 0x14, 0x0f: 0x15, 0x01: 0x16, 0x11: 0x17, 0x20: 0x18, 0x09: 0x19, 0x0d: 0x1a, 0x07: 0x1b,
        0x10: 0x1c, 0x06: 0x1d, 0x12: 0x1e, 0x13: 0x1f, 0x14: 0x20, 0x15: 0x21, 0x17: 0x22, 0x16: 0x23,
        0x1a: 0x24, 0x1c: 0x25, 0x19: 0x26, 0x1d: 0x27, 0x24: 0x28, 0x4c: 0x28, 0x35: 0x29, 0x33: 0x2a,
        0x30: 0x2b, 0x31: 0x2c, 0x1b: 0x2d, 0x18: 0x2e, 0x21: 0x2f, 0x1e: 0x30, 0x2a: 0x31, 0x29: 0x33,
        0x27: 0x34, 0x32: 0x35, 0x2b: 0x36, 0x2f: 0x37, 0x2c: 0x38, 0x75: 0x4c, 0x7c: 0x4f, 0x7b: 0x50,
        0x7d: 0x51, 0x7e: 0x52, 0x73: 0x4a, 0x77: 0x4d, 0x74: 0x4b, 0x79: 0x4e,
    ]

    /// Modifier flags to the left-hand modifier usages.
    static let modifiers: [(flag: NSEvent.ModifierFlags, usage: UInt32)] = [
        (.control, 0xe0), (.shift, 0xe1), (.option, 0xe2),
    ]
}

// MARK: - The phone

/// Glass the live screen can be drawn on.
struct Glass {
    /// Its nodes. The Duo's inner screen is two: the half that stays and,
    /// below it in the tree, the half that folds.
    let nodes: [SCNNode]
    /// Its size in `pivot`'s units, the main screen being 1 tall.
    let size: CGSize

    func contains(_ node: SCNNode) -> Bool { nodes.contains { $0 === node } }
}

/// The iPhone Duo's hinge. Its left half turns about the hinge onto the
/// right; folded, the cover on the back of that half faces front.
struct Fold {
    /// Turn these about Y from 0 (open) to π (folded).
    let hinges: [SCNNode]
    /// Moves so the glass in use stays centred.
    let holder: SCNNode
    let cover: Glass
    let openPosition: SIMD3<Float>
    let foldedPosition: SIMD3<Float>
    let openSize: CGSize
    let foldedSize: CGSize
}

/// The 3D phone: leaf's iPhone model, turned to face +Z with its screen
/// centred on the origin and 1 unit tall, and the screen mesh given UVs that
/// span the display, (0, 0) at its top left.
struct Phone {
    let scene: SCNScene
    /// Turn this to turn the phone.
    let pivot: SCNNode
    /// The main screen. In `pivot`'s space it spans ±width/2 across and
    /// ±0.5 up and down, at z = 0.
    let screen: Glass
    /// Shows the simulator. Glass not in use is `dark`.
    let material: SCNMaterial
    let dark: SCNMaterial
    /// The whole phone's size, screen heights.
    let size: CGSize
    /// Present on the iPhone Duo.
    let fold: Fold?

    /// What leaf needs to know about one of its phone models.
    struct Model {
        /// The screen's material.
        let screenMaterial: String
        /// Prefixes of the nodes that sit on the screen (the Dynamic
        /// Island), which are lifted just above it.
        var onScreen: [String] = []
        /// Which way the screen faces in its mesh's own space.
        var facing: Facing = .plusZ
        /// The file holds more than one phone (a showcase of front and
        /// back); every mesh beside the screen's phone is dropped.
        var showcase = false

        enum Facing { case plusZ, minusZ }
    }

    /// leaf's models, by file name (Rust's `Body::model` picks one).
    static let models: [String: Model] = [
        "iphone.usdz": Model(screenMaterial: "COLOUR_Cherry_Screen", onScreen: ["Front_Cam", "Front_Sensor"]),
        "iphone-duo-open.usdz": Model(screenMaterial: "COLOUR_Cherry_Screen", onScreen: ["Front_Cam", "Front_Sensor"]),
        "iphone-air.usdz": Model(screenMaterial: "_7Air_Screen", facing: .minusZ),
        "iphone-17-pro.usdz": Model(screenMaterial: "Screen", showcase: true),
        "iphone-16.usdz": Model(screenMaterial: "HdvBvHLgXAUNOwl"),
    ]
    /// The Duo's cover display, on the back of its left half.
    static let coverMaterial = "C_NS_Screen_Outer"
    /// The Duo's hinge line, in the model's own space: x = 0, z = this. Its
    /// open and folded models differ by half a turn about it.
    static let hingeZ: Float = 0.002584

    /// A surface's look: its base colour and how metal and rough it is.
    struct Finish {
        let red: CGFloat, green: CGFloat, blue: CGFloat
        let metalness: CGFloat, roughness: CGFloat

        static func gray(_ level: CGFloat, metalness: CGFloat, roughness: CGFloat) -> Finish {
            Finish(red: level, green: level, blue: level, metalness: metalness, roughness: roughness)
        }

        func apply(to material: SCNMaterial) {
            material.diffuse.contents = NSColor(srgbRed: red, green: green, blue: blue, alpha: 1)
            material.metalness.contents = metalness
            material.roughness.contents = roughness
        }
    }

    /// The Duo's black finish, by material. The model's bake kept only its
    /// sides' colour; every other body part came out the default near-white.
    static let duoFinish: [String: Finish] = {
        let black = Finish.gray(0.012, metalness: 0, roughness: 0.4)
        let backGlass = Finish(red: 0.028, green: 0.03, blue: 0.036, metalness: 0.15, roughness: 0.32)
        let frame = Finish(red: 0.06, green: 0.065, blue: 0.075, metalness: 1, roughness: 0.22)
        let darkMetal = Finish.gray(0.14, metalness: 1, roughness: 0.3)
        return [
            "BASE_Black_Front_Border": black,
            "BASE_Black_Plastic": black,
            "BASE_Black_Pure": black,
            "BASE_Glass_Black": black,
            "BASE_Cam_Edge_Black": black,
            "C_NS_Plastic": black,
            "C_NS_Antenna": Finish.gray(0.03, metalness: 0, roughness: 0.6),
            "C_NS_ButtonCap": frame,
            "C_NS_Side": frame,
            "C_NS_Side_001": frame,
            "C_NS_Side_Matte": Finish(red: 0.06, green: 0.065, blue: 0.075, metalness: 1, roughness: 0.4),
            "C_NS_Backpanel": backGlass,
            "C_NS_Backpanel_Inner": backGlass,
            "C_NS_Apple": Finish.gray(0.1, metalness: 1, roughness: 0.15),
            "BASE_Hinge": Finish.gray(0.04, metalness: 1, roughness: 0.35),
            "BASE_Grey": Finish.gray(0.08, metalness: 0, roughness: 0.8),
            "C_NS_BackCamSpeaker": black,
            "C_NS_BottomTopSpeaker": black,
            "C_NS_Screws": darkMetal,
            "C_NS_Screws_Aniso": darkMetal,
            "BASE_Flash_Base": darkMetal,
            "BASE_Copper": darkMetal,
        ]
    }()

    static func load(_ url: URL, foldable: Bool) -> Result<Phone, SimFailure> {
        guard let model = models[url.lastPathComponent] else {
            return .failure(SimFailure(message: "leaf doesn't know the phone model \(url.lastPathComponent). Reinstall leaf; if it persists, please report it."))
        }
        let screenMaterial = model.screenMaterial
        let scene: SCNScene
        do { scene = try SCNScene(url: url, options: nil) } catch {
            return .failure(SimFailure(message: "leaf's phone model at \(url.path) couldn't be read (\(error.localizedDescription)). Reinstall leaf to restore it."))
        }
        if foldable {
            scene.rootNode.enumerateHierarchy { node, _ in
                for material in node.geometry?.materials ?? [] {
                    if let name = material.name, let finish = duoFinish[name] { finish.apply(to: material) }
                }
            }
        }
        let backward = model.facing == .minusZ
        guard let screen = node(in: scene, material: screenMaterial),
              let (width, height) = mapUVs(screen, mirrored: backward) else {
            return .failure(SimFailure(message: "leaf's phone model at \(url.path) has no screen mesh (material \(screenMaterial)). Reinstall leaf to restore it."))
        }
        let (low, high) = screen.boundingBox
        let material = SCNMaterial()
        material.lightingModel = .constant
        material.diffuse.contents = NSColor.black
        material.diffuse.mipFilter = .none
        material.diffuse.magnificationFilter = .linear
        material.diffuse.minificationFilter = .linear
        material.diffuse.wrapS = .clamp
        material.diffuse.wrapT = .clamp
        // Letterboxing: a mask in the glass's own UVs blacks out around the picture.
        material.multiply.contents = NSColor.white
        material.isDoubleSided = false
        let dark = SCNMaterial()
        dark.lightingModel = .constant
        dark.diffuse.contents = NSColor.black
        screen.geometry?.materials = [material]

        var cover: (node: SCNNode, width: Float, height: Float)?
        if foldable {
            guard let node = node(in: scene, material: coverMaterial),
                  let (width, height) = mapUVs(node, mirrored: true) else {
                return .failure(SimFailure(message: "leaf's iPhone Duo model at \(url.path) has no cover display (material \(coverMaterial)). Reinstall leaf to restore it."))
            }
            node.geometry?.materials = [dark]
            cover = (node, width, height)
        }

        if model.showcase { keepScreensPhone(scene, screen: screen) }

        // The screen and the island share a depth; lift each a hair (in
        // screen heights, as the models' units differ) so the island stays
        // on top and nothing flickers.
        screen.simdLocalTranslate(by: SIMD3(0, 0, (backward ? -height : height) * 0.00006))
        scene.rootNode.enumerateHierarchy { node, _ in
            if let name = node.name, model.onScreen.contains(where: name.hasPrefix) {
                node.simdLocalTranslate(by: SIMD3(0, 0, height * 0.00013))
            }
        }

        let center = SIMD3<Float>(Float(low.x + high.x) / 2, Float(low.y + high.y) / 2, Float(high.z))
        var move = matrix_identity_float4x4
        move.columns.3 = SIMD4(-center, 1)
        let scale = simd_float4x4(diagonal: SIMD4(1 / height, 1 / height, 1 / height, 1))
        let holder = SCNNode()
        let worldToScreen = screen.simdWorldTransform.inverse
        for child in scene.rootNode.childNodes { holder.addChildNode(child) }
        // A screen facing -Z is turned half a turn about Y to face the camera.
        let toFront = backward ? simd_float4x4(simd_quatf(angle: .pi, axis: SIMD3(0, 1, 0))) : matrix_identity_float4x4
        holder.simdTransform = toFront * scale * move * worldToScreen
        let pivot = SCNNode()
        pivot.addChildNode(holder)
        scene.rootNode.addChildNode(pivot)
        // Every mesh's box corners in `pivot`'s space. A node's own bounding
        // box doesn't follow its children's turns until a frame draws.
        func bodySize() -> CGSize {
            var low = SIMD2<Float>(repeating: .infinity), high = SIMD2<Float>(repeating: -.infinity)
            pivot.enumerateHierarchy { node, _ in
                guard node.geometry != nil else { return }
                let (a, b) = node.boundingBox
                for corner in 0..<8 {
                    let local = SIMD3<Float>(Float(corner & 1 == 0 ? a.x : b.x), Float(corner & 2 == 0 ? a.y : b.y),
                                             Float(corner & 4 == 0 ? a.z : b.z))
                    let point = pivot.simdConvertPosition(local, from: node)
                    low = simd_min(low, SIMD2(point.x, point.y))
                    high = simd_max(high, SIMD2(point.x, point.y))
                }
            }
            return low.x <= high.x ? CGSize(width: CGFloat(high.x - low.x), height: CGFloat(high.y - low.y)) : .zero
        }
        let main = Glass(nodes: [screen], size: CGSize(width: CGFloat(width / height), height: 1))

        guard let cover else {
            return .success(Phone(scene: scene, pivot: pivot, screen: main, material: material, dark: dark,
                                  size: bodySize(), fold: nil))
        }
        let (hinges, moved) = splitAtHinge(holder)
        let openSize = bodySize()
        let openPosition = holder.simdPosition
        for hinge in hinges { hinge.simdEulerAngles.y = .pi }
        let coverNode = moved[ObjectIdentifier(cover.node)] ?? cover.node
        let (coverLow, coverHigh) = coverNode.boundingBox
        let coverCenter = pivot.simdConvertPosition((SIMD3<Float>(coverLow) + SIMD3<Float>(coverHigh)) / 2, from: coverNode)
        let foldedSize = bodySize()
        for hinge in hinges { hinge.simdEulerAngles.y = 0 }
        let fold = Fold(hinges: hinges, holder: holder,
                        cover: Glass(nodes: [coverNode],
                                     size: CGSize(width: CGFloat(cover.width / height), height: CGFloat(cover.height / height))),
                        openPosition: openPosition, foldedPosition: openPosition - coverCenter,
                        openSize: openSize, foldedSize: foldedSize)
        let screenNodes = [screen] + (moved[ObjectIdentifier(screen)].map { [$0] } ?? [])
        return .success(Phone(scene: scene, pivot: pivot, screen: Glass(nodes: screenNodes, size: main.size),
                              material: material, dark: dark, size: openSize, fold: fold))
    }

    /// Drops every mesh that sits beside the screen's phone rather than
    /// within it: a showcase file's other phones.
    static func keepScreensPhone(_ scene: SCNScene, screen: SCNNode) {
        let (low, high) = screen.boundingBox
        let margin = Float(high.x - low.x) * 0.25
        let left = Float(low.x) - margin, right = Float(high.x) + margin
        var beside: [SCNNode] = []
        var screenAndAncestors: [SCNNode] = []
        var walk: SCNNode? = screen
        while let next = walk { screenAndAncestors.append(next); walk = next.parent }
        scene.rootNode.enumerateHierarchy { node, _ in
            guard node.geometry != nil, !screenAndAncestors.contains(where: { $0 === node }) else { return }
            let (a, b) = node.boundingBox
            let middle = screen.simdConvertPosition((SIMD3<Float>(a) + SIMD3<Float>(b)) / 2, from: node)
            if middle.x < left || middle.x > right { beside.append(node) }
        }
        for node in beside { node.removeFromParentNode() }
    }

    /// The first node drawn with `material`.
    static func node(in scene: SCNScene, material: String) -> SCNNode? {
        var found: SCNNode?
        scene.rootNode.enumerateHierarchy { node, stop in
            if node.geometry?.materials.contains(where: { $0.name == material }) == true {
                found = node
                stop.pointee = true
            }
        }
        return found
    }

    /// Gives `node`'s flat mesh UVs straight across it, from its own XY
    /// plane, (0, 0) at the top left; `mirrored` for glass that faces -Z.
    /// Returns its width and height, or nil for a mesh it can't read.
    static func mapUVs(_ node: SCNNode, mirrored: Bool) -> (Float, Float)? {
        guard let geometry = node.geometry, let vertices = geometry.sources(for: .vertex).first,
              vertices.usesFloatComponents, vertices.bytesPerComponent == 4, vertices.componentsPerVector >= 2 else { return nil }
        let (low, high) = node.boundingBox
        let width = Float(high.x - low.x), height = Float(high.y - low.y)
        guard width > 0, height > 0 else { return nil }
        var uvs: [SIMD2<Float>] = []
        uvs.reserveCapacity(vertices.vectorCount)
        vertices.data.withUnsafeBytes { raw in
            for index in 0..<vertices.vectorCount {
                let base = vertices.dataOffset + index * vertices.dataStride
                let x = raw.loadUnaligned(fromByteOffset: base, as: Float.self)
                let y = raw.loadUnaligned(fromByteOffset: base + 4, as: Float.self)
                let u = mirrored ? (Float(high.x) - x) / width : (x - Float(low.x)) / width
                uvs.append(SIMD2(u, (Float(high.y) - y) / height))
            }
        }
        let uvSource = SCNGeometrySource(data: uvs.withUnsafeBufferPointer { Data(buffer: $0) }, semantic: .texcoord,
                                         vectorCount: uvs.count, usesFloatComponents: true, componentsPerVector: 2,
                                         bytesPerComponent: 4, dataOffset: 0, dataStride: MemoryLayout<SIMD2<Float>>.stride)
        // The new UVs follow the positions' indices.
        let kept = geometry.sources.indices.filter { geometry.sources[$0].semantic != .texcoord }
        let channels = geometry.geometrySourceChannels.map { channels in
            kept.map { channels[$0] } + [channels[geometry.sources.firstIndex { $0 === vertices } ?? 0]]
        }
        let mapped = SCNGeometry(sources: kept.map { geometry.sources[$0] } + [uvSource], elements: geometry.elements,
                                 sourceChannels: channels)
        mapped.materials = geometry.materials
        node.geometry = mapped
        return (width, height)
    }

    /// Splits every mesh under `root` at the hinge: triangles left of it move
    /// onto a node that turns about the hinge line. Returns those turning
    /// nodes, and the node each split mesh's left half went to, by the
    /// original node.
    static func splitAtHinge(_ root: SCNNode) -> ([SCNNode], [ObjectIdentifier: SCNNode]) {
        var meshes: [SCNNode] = []
        root.enumerateHierarchy { node, _ in if node.geometry != nil { meshes.append(node) } }
        var hinges: [SCNNode] = []
        var moved: [ObjectIdentifier: SCNNode] = [:]
        for node in meshes {
            guard let geometry = node.geometry, let (points, channel) = positions(geometry) else { continue }
            var stay: [(SCNGeometryElement, SCNMaterial?)] = [], turn: [(SCNGeometryElement, SCNMaterial?)] = []
            // The halves share the vertex data, so each gets the box of the
            // corners it uses rather than the whole mesh's.
            var stayBox = Box(), turnBox = Box()
            for (index, part) in geometry.elements.enumerated() {
                let material = geometry.materials.isEmpty ? nil : geometry.materials[index % geometry.materials.count]
                guard part.primitiveType == .triangles, let triangles = Triangles(part) else {
                    stay.append((part, material))
                    stayBox.add(SIMD3<Float>(geometry.boundingBox.min))
                    stayBox.add(SIMD3<Float>(geometry.boundingBox.max))
                    continue
                }
                var left: [Int] = [], right: [Int] = []
                for triangle in 0..<triangles.count {
                    let corners = (0..<3).compactMap { corner -> SIMD3<Float>? in
                        let vertex = Int(triangles.index(triangle, corner, channel))
                        return vertex < points.count ? points[vertex] : nil
                    }
                    let middle = corners.reduce(Float(0)) { $0 + $1.x } / Float(max(corners.count, 1))
                    if middle < 0 {
                        left.append(triangle)
                        corners.forEach { turnBox.add($0) }
                    } else {
                        right.append(triangle)
                        corners.forEach { stayBox.add($0) }
                    }
                }
                if !right.isEmpty { stay.append((triangles.element(right), material)) }
                if !left.isEmpty { turn.append((triangles.element(left), material)) }
            }
            guard !turn.isEmpty else { continue }
            let flap = SCNNode(geometry: rebuilt(geometry, turn, turnBox))
            node.geometry = stay.isEmpty ? nil : rebuilt(geometry, stay, stayBox)
            let hinge = SCNNode()
            hinge.simdPosition = SIMD3(0, 0, hingeZ)
            flap.simdPosition = SIMD3(0, 0, -hingeZ)
            hinge.addChildNode(flap)
            node.addChildNode(hinge)
            hinges.append(hinge)
            moved[ObjectIdentifier(node)] = flap
        }
        return (hinges, moved)
    }

    /// A bounding box grown one point at a time.
    struct Box {
        var low = SIMD3<Float>(repeating: .infinity)
        var high = SIMD3<Float>(repeating: -.infinity)

        mutating func add(_ point: SIMD3<Float>) {
            low = simd_min(low, point)
            high = simd_max(high, point)
        }
    }

    private static func rebuilt(_ geometry: SCNGeometry, _ parts: [(SCNGeometryElement, SCNMaterial?)], _ box: Box) -> SCNGeometry {
        let result = SCNGeometry(sources: geometry.sources, elements: parts.map(\.0),
                                 sourceChannels: geometry.geometrySourceChannels)
        result.materials = parts.compactMap(\.1)
        if box.low.x <= box.high.x { result.boundingBox = (SCNVector3(box.low), SCNVector3(box.high)) }
        return result
    }

    /// Each vertex's position, and the index channel positions use; nil for
    /// a vertex format this can't read.
    private static func positions(_ geometry: SCNGeometry) -> ([SIMD3<Float>], Int)? {
        guard let vertices = geometry.sources(for: .vertex).first, vertices.usesFloatComponents,
              vertices.bytesPerComponent == 4, vertices.componentsPerVector >= 3 else { return nil }
        let channel = geometry.sources.firstIndex { $0 === vertices }
            .flatMap { index in geometry.geometrySourceChannels.map { $0[index].intValue } } ?? 0
        let points = vertices.data.withUnsafeBytes { raw in
            (0..<vertices.vectorCount).map { index -> SIMD3<Float> in
                let base = vertices.dataOffset + index * vertices.dataStride
                return SIMD3(raw.loadUnaligned(fromByteOffset: base, as: Float.self),
                             raw.loadUnaligned(fromByteOffset: base + 4, as: Float.self),
                             raw.loadUnaligned(fromByteOffset: base + 8, as: Float.self))
            }
        }
        return (points, channel)
    }
}

/// A triangle element's indices, one or more channels of them (positions,
/// normals and UVs can each have their own), interleaved per corner or not.
struct Triangles {
    let count: Int
    let channels: Int
    let interleaved: Bool
    /// Every index, widened, in the element's own order.
    let words: [UInt32]

    init?(_ element: SCNGeometryElement) {
        count = element.primitiveCount
        channels = max(1, element.indicesChannelCount)
        interleaved = element.hasInterleavedIndicesChannels
        let total = count * 3 * channels
        let size = element.bytesPerIndex
        guard [1, 2, 4].contains(size), element.data.count >= total * size else { return nil }
        words = element.data.withUnsafeBytes { raw in
            (0..<total).map { index -> UInt32 in
                switch size {
                case 1: return UInt32(raw.load(fromByteOffset: index, as: UInt8.self))
                case 2: return UInt32(raw.loadUnaligned(fromByteOffset: index * 2, as: UInt16.self))
                default: return raw.loadUnaligned(fromByteOffset: index * 4, as: UInt32.self)
                }
            }
        }
    }

    /// Where corner `corner` of `triangle`'s index in `channel` sits in `words`.
    private func offset(_ triangle: Int, _ corner: Int, _ channel: Int) -> Int {
        interleaved ? (triangle * 3 + corner) * channels + channel : channel * count * 3 + triangle * 3 + corner
    }

    func index(_ triangle: Int, _ corner: Int, _ channel: Int) -> UInt32 {
        words[offset(triangle, corner, min(channel, channels - 1))]
    }

    /// An element of just `triangles`, laid out like this one.
    func element(_ triangles: [Int]) -> SCNGeometryElement {
        var picked: [UInt32] = []
        picked.reserveCapacity(triangles.count * 3 * channels)
        if interleaved {
            for triangle in triangles {
                for corner in 0..<3 { for channel in 0..<channels { picked.append(index(triangle, corner, channel)) } }
            }
        } else {
            for channel in 0..<channels {
                for triangle in triangles { for corner in 0..<3 { picked.append(index(triangle, corner, channel)) } }
            }
        }
        return SCNGeometryElement(data: picked.withUnsafeBufferPointer { Data(buffer: $0) }, primitiveType: .triangles,
                                  primitiveCount: triangles.count, indicesChannelCount: channels,
                                  interleavedIndicesChannels: interleaved, bytesPerIndex: 4)
    }
}

/// The phone's turn and the camera's distance, eased toward their targets.
struct Orbit {
    var yaw: CGFloat = -0.32
    var pitch: CGFloat = 0.1
    var velocity = CGPoint.zero
    var zoom: CGFloat = 1
    var front = false
    var lastTouched = Date.distantPast

    static let pitchLimit: CGFloat = 1.25

    mutating func turn(by delta: CGPoint) {
        yaw += delta.x
        pitch = min(max(pitch + delta.y, -Orbit.pitchLimit), Orbit.pitchLimit)
        lastTouched = Date()
    }
}

/// The SceneKit view the phone is drawn in; it passes mouse and keys to the
/// simulator pane, which decides what they do.
final class PhoneView: SCNView {
    weak var owner: Simulator?

    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with event: NSEvent) { owner?.mouseDown(event) }
    override func mouseDragged(with event: NSEvent) { owner?.mouseDragged(event) }
    override func mouseUp(with event: NSEvent) { owner?.mouseUp(event) }
    override func scrollWheel(with event: NSEvent) { owner?.scrolled(event) }
    override func magnify(with event: NSEvent) { owner?.magnified(event) }
    override func keyDown(with event: NSEvent) {
        if owner?.keyDown(event) != true { super.keyDown(with: event) }
    }
    override func keyUp(with event: NSEvent) {
        if owner?.keyUp(event) != true { super.keyUp(with: event) }
    }
    override func flagsChanged(with event: NSEvent) { owner?.flagsChanged(event) }

    override func resetCursorRects() {
        if owner?.marking == true { addCursorRect(bounds, cursor: .crosshair) }
    }
}

/// The rectangle being marked on the screen to ask about.
final class Marquee: NSView {
    override func hitTest(_ point: NSPoint) -> NSView? { nil }
    override func draw(_ dirtyRect: NSRect) {
        NSColor.controlAccentColor.withAlphaComponent(0.16).setFill()
        bounds.fill()
        NSColor.controlAccentColor.setStroke()
        let path = NSBezierPath(rect: bounds.insetBy(dx: 0.75, dy: 0.75))
        path.lineWidth = 1.5
        path.stroke()
    }
}

/// The body drawn around the simulator's screen.
enum Chassis {
    /// The single-screen iPhone.
    case phone
    /// The foldable iPhone Duo, folded or open.
    case duo(Pose)

    enum Pose { case folded, open }
}

/// The card at the bottom: the question about the screen, and what became of it.
final class AskCard: NSView {
    let field = NSTextField()
    let note = NSTextField(labelWithString: "")
    let send: ToolButton
    override var isFlipped: Bool { true }

    init(target: AnyObject, action: Selector) {
        send = ToolButton(symbol: "arrow.up.circle.fill", label: "Send", target: target, action: action)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 12
        layer?.cornerCurve = .continuous
        layer?.borderWidth = 1
        layer?.shadowOpacity = 0.12
        layer?.shadowRadius = 14
        layer?.shadowOffset = CGSize(width: 0, height: -4)
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: 13)
        field.usesSingleLineMode = true
        field.cell?.isScrollable = true
        field.target = target
        field.action = action
        addSubview(field)
        send.image = NSImage(systemSymbolName: "arrow.up.circle.fill", accessibilityDescription: "Send")?
            .withSymbolConfiguration(.init(pointSize: 18, weight: .regular))
        addSubview(send)
        note.font = .systemFont(ofSize: 11)
        note.textColor = .secondaryLabelColor
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

    static let height: CGFloat = 10 + 26 + 4 + 16 + 10

    override func layout() {
        super.layout()
        field.frame = NSRect(x: 14, y: 14, width: bounds.width - 14 - 44, height: 18)
        send.frame = NSRect(x: bounds.width - 40, y: 9, width: 30, height: 28)
        note.frame = NSRect(x: 14, y: 10 + 26 + 4, width: bounds.width - 28, height: 16)
    }
}

// MARK: - Simulator pane

final class Simulator: PaneController, SCNSceneRendererDelegate, NSTextFieldDelegate {
    let udid: String
    let name: String
    let runtime: String
    let developerDir: String

    let bar = Bar()
    let titleLabel = NSTextField(labelWithString: "")
    let home: ToolButton
    let lock: ToolButton
    let turn: ToolButton
    let fold: ToolButton
    let ask: ToolButton
    let close: ToolButton
    let phoneView = PhoneView(frame: .zero, options: nil)
    let status = NSStackView()
    let spinner = NSProgressIndicator()
    let statusLabel = NSTextField(labelWithString: "")
    let marquee = Marquee()
    var askCard: AskCard!

    var phone: Phone?
    /// The body the screen is drawn on. The Duo starts folded, as its iOS does.
    var chassis: Chassis
    /// True while the Duo folds or opens; touches wait for it.
    var folding = false
    var orbit = Orbit()
    var device: NSObject?
    var display: NSObject?
    /// The Duo's two screens, once both are up.
    var duoDisplays: (cover: NSObject, inner: NSObject)?
    /// The Duo's hinge helper's stdin, while it runs.
    var hingeInput: FileHandle?
    var lastHingeAngle: Double?
    var surface: IOSurface?
    var hid: HID?
    var bootedHere = false
    var server: ControlServer?
    var dragging: Dragging = .none
    var heldModifiers: NSEvent.ModifierFlags = []

    // Asking the agent about the screen
    var marking = false
    var askShown = false
    var mark: CGRect?
    var markStart: CGPoint?
    var asking = false
    var agentName: String = {
        let name = ProcessInfo.processInfo.environment["LEAF_AGENT"] ?? ""
        return name.isEmpty ? "your agent" : name
    }()

    /// What a mouse drag that started in the pane is doing.
    enum Dragging {
        case none
        /// A finger on the screen, last at this point (fractions of the screen).
        case touch(CGPoint)
        /// Turning the phone, last at this window point.
        case turning(CGPoint)
        /// Marking part of the screen to ask about, from this view point.
        case marking(CGPoint)
    }

    static let barHeight: CGFloat = 40

    init(session: String, block: String, udid: String, name: String, runtime: String, developerDir: String) {
        self.udid = udid
        self.name = name
        self.runtime = runtime
        self.developerDir = developerDir
        home = ToolButton(symbol: "house", label: "Home (⇧⌘H)", target: nil, action: #selector(Simulator.pressHome))
        lock = ToolButton(symbol: "lock", label: "Lock (⌘L)", target: nil, action: #selector(Simulator.pressLock))
        turn = ToolButton(symbol: "rotate.3d", label: "Face Front (⌘0)", target: nil, action: #selector(Simulator.toggleFront))
        fold = ToolButton(symbol: "book", label: "Expand iPhone Duo (⌘F)", target: nil, action: #selector(Simulator.toggleFold))
        ask = ToolButton(symbol: "text.bubble", label: "Ask", target: nil, action: #selector(Simulator.toggleAsk))
        close = ToolButton(symbol: "xmark", label: "Close Pane (⌘W)", target: nil, action: #selector(Simulator.closePane))
        chassis = ProcessInfo.processInfo.environment["LEAF_SIM_DUO"] == "1" ? .duo(.folded) : .phone
        super.init(session: session, block: block)
        for button in [home, lock, turn, fold, ask, close] { button.target = self }
        askCard = AskCard(target: self, action: #selector(sendAsk))
        build()
    }

    // MARK: Building

    func build() {
        window.isReleasedWhenClosed = false
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = false
        window.level = .normal
        window.collectionBehavior = [.transient, .fullScreenAuxiliary, .ignoresCycle]
        window.title = "Leaf Simulator"
        window.animationBehavior = .none

        root.wantsLayer = true
        root.layer?.masksToBounds = true
        root.layer?.cornerCurve = .continuous
        window.contentView = root

        phoneView.owner = self
        phoneView.backgroundColor = .clear
        phoneView.antialiasingMode = .multisampling4X
        phoneView.autoenablesDefaultLighting = true
        phoneView.allowsCameraControl = false
        phoneView.rendersContinuously = true
        phoneView.preferredFramesPerSecond = NSScreen.main?.maximumFramesPerSecond ?? 60
        phoneView.delegate = self
        root.addSubview(phoneView)
        window.initialFirstResponder = phoneView

        marquee.isHidden = true
        root.addSubview(marquee)

        root.addSubview(bar)
        titleLabel.font = .systemFont(ofSize: 12.5, weight: .medium)
        titleLabel.textColor = .labelColor
        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.stringValue = runtime.isEmpty ? name : "\(name) · \(runtime)"
        bar.addSubview(titleLabel)
        for button in [home, lock, turn, fold, ask, close] { bar.addSubview(button) }

        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.isIndeterminate = true
        statusLabel.font = .systemFont(ofSize: 12)
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.lineBreakMode = .byTruncatingTail
        statusLabel.maximumNumberOfLines = 3
        status.orientation = .horizontal
        status.spacing = 8
        status.alignment = .centerY
        status.addArrangedSubview(spinner)
        status.addArrangedSubview(statusLabel)
        root.addSubview(status)

        askCard.isHidden = true
        askCard.field.delegate = self
        root.addSubview(askCard)
        nameAgent()

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

    override var focusTarget: NSView? { phoneView }

    /// Simulator shortcuts that stay here even when Rex binds them.
    static let reserved: Set<Chord> = Set([
        "cmd+w", "cmd+q", "shift+cmd+h", "cmd+l", "cmd+v", "cmd+0", "cmd+=", "cmd+-", "cmd+f", "shift+cmd+a",
    ].compactMap(Chord.init))

    override var reservedKeys: Set<Chord> { Simulator.reserved }

    override func applyAppearance() {
        root.effectiveAppearance.performAsCurrentDrawingAppearance {
            root.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        }
        bar.needsDisplay = true
        askCard.applyAppearance()
    }

    func layout() {
        let width = root.bounds.width, height = root.bounds.height
        let barHeight = Simulator.barHeight
        bar.frame = NSRect(x: 0, y: height - barHeight, width: width, height: barHeight)
        let size: CGFloat = 28, top = (barHeight - size) / 2
        var x = width - 8 - size
        for button in [close, ask, fold, turn, lock, home] {
            let shown = button !== fold || pose != nil
            button.isHidden = !shown
            guard shown else { continue }
            button.frame = NSRect(x: x, y: top, width: size, height: size)
            x -= size + 2
        }
        titleLabel.frame = NSRect(x: 14, y: (barHeight - 17) / 2, width: max(40, x - 14), height: 17)
        phoneView.frame = NSRect(x: 0, y: 0, width: width, height: max(0, height - barHeight))
        let cardWidth = min(520, width - 24)
        askCard.frame = NSRect(x: (width - cardWidth) / 2, y: 12, width: cardWidth, height: AskCard.height)
        let statusWidth = min(420, width - 32)
        let statusY: CGFloat = askShown ? 12 + AskCard.height + 10 : 16
        status.frame = NSRect(x: (width - statusWidth) / 2, y: statusY, width: statusWidth, height: 34)
        status.alignment = .centerY
        frameCamera()
    }

    func updateControls() {
        let live = hid != nil
        home.isEnabled = live
        lock.isEnabled = live
        ask.isEnabled = true
        turn.setSymbol(orbit.front ? "rotate.3d" : "iphone", label: orbit.front ? "Turn in 3D (⌘0)" : "Face Front (⌘0)")
        let open = pose == .open
        fold.setSymbol(open ? "book.closed" : "book",
                       label: open ? "Fold iPhone Duo (⌘F)" : "Expand iPhone Duo (⌘F)")
        ask.contentTintColor = askShown ? .controlAccentColor : (ask.isEnabled ? .secondaryLabelColor : .tertiaryLabelColor)
    }

    func showStatus(_ text: String, busy: Bool) {
        status.alphaValue = 1
        statusLabel.stringValue = text
        statusLabel.textColor = busy ? .secondaryLabelColor : .labelColor
        spinner.isHidden = !busy
        if busy { spinner.startAnimation(nil) } else { spinner.stopAnimation(nil) }
        status.isHidden = text.isEmpty
    }

    func fail(_ failure: SimFailure) {
        showStatus(failure.message, busy: false)
        statusLabel.textColor = .systemRed
    }

    // MARK: The phone

    /// The Duo's pose; nil on the iPhone.
    var pose: Chassis.Pose? {
        if case .duo(let pose) = chassis { return pose }
        return nil
    }

    /// The glass the simulator is drawn on: the Duo's cover while it's folded.
    var glass: Glass? {
        guard let phone else { return nil }
        if pose == .folded, let fold = phone.fold { return fold.cover }
        return phone.screen
    }

    /// The whole body's size now, screen heights.
    var bodySize: CGSize {
        guard let phone else { return .zero }
        guard let fold = phone.fold else { return phone.size }
        return pose == .folded ? fold.foldedSize : fold.openSize
    }

    /// True when the picture lies a quarter turn from the glass: the Duo's
    /// inner screen is portrait in its framebuffer but held landscape.
    var quarterTurned: Bool {
        guard let glass, let surface else { return false }
        return (surface.width < surface.height) != (glass.size.width < glass.size.height)
    }

    /// Where the picture sits on the glass, as fractions of it, after any
    /// quarter turn. A picture of another shape than the glass is fitted in
    /// the middle with black beside it.
    var picture: CGRect {
        let whole = CGRect(x: 0, y: 0, width: 1, height: 1)
        guard let glass, let surface, surface.width > 0, surface.height > 0 else { return whole }
        let (across, down) = quarterTurned ? (surface.height, surface.width) : (surface.width, surface.height)
        let ratio = (CGFloat(across) / CGFloat(down)) / (glass.size.width / glass.size.height)
        if abs(ratio - 1) < 0.1 { return whole }
        if ratio < 1 { return CGRect(x: (1 - ratio) / 2, y: 0, width: ratio, height: 1) }
        return CGRect(x: 0, y: (1 - 1 / ratio) / 2, width: 1, height: 1 / ratio)
    }

    /// Lights the glass in use and darkens the rest.
    func dressGlass() {
        guard let phone, let live = glass else { return }
        for glass in [phone.screen] + (phone.fold.map { [$0.cover] } ?? []) {
            let lit = glass.nodes.first === live.nodes.first
            for node in glass.nodes { node.geometry?.materials = [lit ? phone.material : phone.dark] }
        }
        let fit = picture
        let fitted = SCNMatrix4Mult(
            SCNMatrix4MakeTranslation(-fit.minX, -fit.minY, 0), SCNMatrix4MakeScale(1 / fit.width, 1 / fit.height, 1))
        phone.material.diffuse.contentsTransform = quarterTurned ? SCNMatrix4Mult(fitted, Simulator.quarterTurn) : fitted
        guard fit != CGRect(x: 0, y: 0, width: 1, height: 1) else {
            phone.material.multiply.contents = NSColor.white
            return
        }
        let side: CGFloat = 512
        phone.material.multiply.contents = NSImage(size: NSSize(width: side, height: side), flipped: true) { rect in
            NSColor.black.setFill()
            rect.fill()
            NSColor.white.setFill()
            NSRect(x: fit.minX * side, y: fit.minY * side, width: fit.width * side, height: fit.height * side).fill()
            return true
        }
    }

    /// Takes a point on a landscape picture, (x, y) from its top left, to the
    /// portrait framebuffer under it: (y, 1 - x). iOS holds the open Duo's
    /// inner screen with the framebuffer's top along the picture's right.
    static let quarterTurn: SCNMatrix4 = {
        var turn = SCNMatrix4Identity
        turn.m11 = 0
        turn.m12 = -1
        turn.m21 = 1
        turn.m22 = 0
        turn.m42 = 1
        return turn
    }()

    /// Which body to draw, beside the helper.
    func modelURL() -> URL? {
        guard let executable = Bundle.main.executableURL else { return nil }
        let chosen = ProcessInfo.processInfo.environment["LEAF_SIM_MODEL"] ?? ""
        let name = pose != nil ? "iphone-duo-open.usdz" : (Phone.models[chosen] == nil ? "iphone.usdz" : chosen)
        return executable.deletingLastPathComponent().appendingPathComponent(name)
    }

    func loadPhone() {
        guard let url = modelURL() else { return }
        switch Phone.load(url, foldable: pose != nil) {
        case .success(let phone):
            self.phone = phone
            phoneView.scene = phone.scene
            let camera = SCNCamera()
            camera.fieldOfView = 26
            camera.zNear = 0.01
            camera.zFar = 100
            let cameraNode = SCNNode()
            cameraNode.camera = camera
            phone.scene.rootNode.addChildNode(cameraNode)
            phoneView.pointOfView = cameraNode
            if let fold = phone.fold, pose == .folded {
                for hinge in fold.hinges { hinge.simdEulerAngles.y = .pi }
                fold.holder.simdPosition = fold.foldedPosition
            }
            dressGlass()
            if let surface {
                show(surface)
            } else {
                pulseScreen()
            }
            frameCamera()
            applyOrbit(animated: false)
        case .failure(let failure):
            fail(failure)
        }
    }

    /// Folds or opens the Duo: its left half swings about the hinge, and
    /// iOS's hinge follows. Folded, the cover faces front; open, the inner
    /// screen does. Folding shut locks the phone, as on the device; a tap on
    /// the cover wakes it.
    @objc func toggleFold() {
        guard let pose, let phone, let fold = phone.fold, !folding else { return }
        let opening = pose == .folded
        if case .touch(let last) = dragging {
            hid?.touch(.up, at: last)
            dragging = .none
        }
        folding = true
        chassis = .duo(opening ? .open : .folded)
        updateControls()
        let duration: CGFloat = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion ? 0.05 : 0.9
        let from: Float = opening ? .pi : 0, to: Float = opening ? 0 : .pi
        let start = fold.holder.simdPosition, end = opening ? fold.openPosition : fold.foldedPosition
        var swapped = false
        let swing = SCNAction.customAction(duration: duration) { [weak self] _, elapsed in
            let t = Float(min(1, elapsed / duration))
            let eased = t < 0.5 ? 4 * t * t * t : 1 - pow(2 - 2 * t, 3) / 2
            for hinge in fold.hinges { hinge.simdEulerAngles.y = from + (to - from) * eased }
            fold.holder.simdPosition = start + (end - start) * eased
            // iOS's hinge follows the model's, so its own fold transition
            // plays in step.
            let degrees = Double(opening ? eased : 1 - eased) * 180
            DispatchQueue.main.async { self?.moveHinge(to: degrees, settled: false) }
            // Halfway, the glass that ends up in front takes the picture.
            if !swapped, eased >= 0.5 {
                swapped = true
                DispatchQueue.main.async {
                    self?.showDuoScreen()
                    self?.dressGlass()
                }
            }
        }
        phone.pivot.runAction(swing, forKey: "fold") { [weak self] in
            DispatchQueue.main.async {
                guard let self else { return }
                self.moveHinge(to: opening ? 180 : 0, settled: true)
                self.folding = false
                self.dressGlass()
                if let surface = self.surface { self.routeTouches(to: surface) }
            }
        }
        SCNTransaction.begin()
        SCNTransaction.animationDuration = Double(duration)
        SCNTransaction.animationTimingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        frameCamera()
        SCNTransaction.commit()
    }

    /// The dark screen breathes while the simulator starts.
    func pulseScreen() {
        guard let phone else { return }
        let period = 2.4
        let material = phone.material
        let pulse = SCNAction.customAction(duration: period) { _, elapsed in
            let phase = (1 - cos(Double(elapsed) / period * 2 * .pi)) / 2
            material.diffuse.contents = NSColor(white: 0.04 + 0.08 * phase, alpha: 1)
        }
        phone.pivot.runAction(.repeatForever(pulse), forKey: "loading")
    }

    /// Moves the camera back far enough that the whole phone fits, with room.
    func frameCamera() {
        guard phone != nil, let camera = phoneView.pointOfView, phoneView.bounds.width > 0, phoneView.bounds.height > 0 else { return }
        let aspect = phoneView.bounds.width / phoneView.bounds.height
        let tangent = tan(26 * CGFloat.pi / 180 / 2)
        let margin: CGFloat = 1.14
        let forHeight = bodySize.height * margin / (2 * tangent)
        let forWidth = bodySize.width * margin / (2 * tangent * aspect)
        let distance = max(forHeight, forWidth) / orbit.zoom
        camera.simdPosition = SIMD3(0, 0, Float(distance))
    }

    func applyOrbit(animated: Bool) {
        guard let phone else { return }
        SCNTransaction.begin()
        SCNTransaction.animationDuration = animated ? 0.45 : 0
        SCNTransaction.animationTimingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
        phone.pivot.simdEulerAngles = SIMD3(Float(orbit.pitch), Float(orbit.yaw), 0)
        SCNTransaction.commit()
    }

    /// Idle sway and the coast after a flick, about 60 times a second.
    func animate() {
        // Nothing to draw while the pane is hidden.
        if phoneView.rendersContinuously != shown { phoneView.rendersContinuously = shown }
        guard phone != nil, shown else { return }
        if case .turning = dragging { return }
        let speed = hypot(orbit.velocity.x, orbit.velocity.y)
        if speed > 0.0004 {
            orbit.turn(by: orbit.velocity)
            orbit.velocity = CGPoint(x: orbit.velocity.x * 0.93, y: orbit.velocity.y * 0.93)
            applyOrbit(animated: false)
            return
        }
        orbit.velocity = .zero
        guard !orbit.front, !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion,
              Date().timeIntervalSince(orbit.lastTouched) > 2.5 else { return }
        let time = CACurrentMediaTime()
        phone?.pivot.simdEulerAngles = SIMD3(Float(orbit.pitch + 0.03 * sin(time * 0.5)),
                                             Float(orbit.yaw + 0.06 * sin(time * 0.37)), 0)
    }

    @objc func toggleFront() {
        orbit.front.toggle()
        orbit.velocity = .zero
        if orbit.front {
            orbit.yaw = 0
            orbit.pitch = 0
        } else {
            orbit.yaw = -0.32
            orbit.pitch = 0.1
        }
        orbit.lastTouched = Date()
        applyOrbit(animated: true)
        updateControls()
    }

    @objc func zoomIn() { orbit.zoom = min(3, orbit.zoom * 1.15); frameCamera() }
    @objc func zoomOut() { orbit.zoom = max(0.5, orbit.zoom / 1.15); frameCamera() }

    @objc func validateMenuItem(_ item: NSMenuItem) -> Bool {
        guard item.action == #selector(toggleFold) else { return true }
        item.title = pose == .open ? "Fold iPhone Duo" : "Expand iPhone Duo"
        return pose != nil && !folding
    }

    /// Where `event` lands on the simulator's picture, as fractions from its
    /// top left; nil when it misses. `slide` keeps a finger that's already
    /// down on the screen even when the phone's body is in front.
    func screenPoint(_ event: NSEvent, slide: Bool) -> CGPoint? {
        guard let glass, let root = glass.nodes.first, !folding else { return nil }
        let point = phoneView.convert(event.locationInWindow, from: nil)
        var options: [SCNHitTestOption: Any] = [.ignoreHiddenNodes: true]
        if slide {
            options[.rootNode] = root
        } else {
            options[.searchMode] = SCNHitTestSearchMode.closest.rawValue
        }
        guard let hit = phoneView.hitTest(point, options: options).first, glass.contains(hit.node) else { return nil }
        // SceneKit reports V from the bottom; the glass's UVs start at the top.
        let uv = hit.textureCoordinates(withMappingChannel: 0)
        let fit = picture
        let across = (uv.x - fit.minX) / fit.width, down = (1 - uv.y - fit.minY) / fit.height
        let (x, y) = quarterTurned ? (down, 1 - across) : (across, down)
        if slide { return CGPoint(x: min(max(x, 0), 1), y: min(max(y, 0), 1)) }
        return (0...1).contains(x) && (0...1).contains(y) ? CGPoint(x: x, y: y) : nil
    }

    // MARK: Mouse

    func mouseDown(_ event: NSEvent) {
        lastInputAt = Date()
        window.makeFirstResponder(phoneView)
        if marking {
            let point = phoneView.convert(event.locationInWindow, from: nil)
            dragging = .marking(point)
            markStart = screenPoint(event, slide: false)
            mark = nil
            marquee.isHidden = true
            return
        }
        if let point = screenPoint(event, slide: false), let hid {
            dragging = .touch(point)
            hid.touch(.down, at: point)
            return
        }
        if event.clickCount == 2 {
            toggleFront()
            dragging = .none
            return
        }
        orbit.velocity = .zero
        dragging = .turning(event.locationInWindow)
    }

    func mouseDragged(_ event: NSEvent) {
        lastInputAt = Date()
        switch dragging {
        case .touch(let last):
            let point = screenPoint(event, slide: true) ?? last
            dragging = .touch(point)
            hid?.touch(.moved, at: point)
        case .turning(let last):
            guard !orbit.front else { return }
            let now = event.locationInWindow
            let delta = CGPoint(x: (now.x - last.x) * 0.008, y: -(now.y - last.y) * 0.008)
            orbit.turn(by: delta)
            orbit.velocity = delta
            applyOrbit(animated: false)
            dragging = .turning(now)
        case .marking(let start):
            let now = phoneView.convert(event.locationInWindow, from: nil)
            let rect = NSRect(x: min(start.x, now.x), y: min(start.y, now.y),
                              width: abs(now.x - start.x), height: abs(now.y - start.y))
            marquee.frame = phoneView.convert(rect, to: root)
            marquee.isHidden = rect.width < 4 || rect.height < 4
            marquee.needsDisplay = true
        case .none:
            break
        }
    }

    func mouseUp(_ event: NSEvent) {
        switch dragging {
        case .touch(let last):
            hid?.touch(.up, at: screenPoint(event, slide: true) ?? last)
        case .turning:
            orbit.lastTouched = Date()
        case .marking:
            if let start = markStart, let end = screenPoint(event, slide: true), !marquee.isHidden {
                mark = CGRect(x: min(start.x, end.x), y: min(start.y, end.y),
                              width: abs(end.x - start.x), height: abs(end.y - start.y))
            } else {
                mark = nil
                marquee.isHidden = true
            }
            markStart = nil
            updateAskNote()
            window.makeFirstResponder(askCard.field)
        case .none:
            break
        }
        dragging = .none
    }

    func scrolled(_ event: NSEvent) {
        guard !orbit.front else { return }
        let scale: CGFloat = event.hasPreciseScrollingDeltas ? 0.004 : 0.04
        orbit.turn(by: CGPoint(x: -event.scrollingDeltaX * scale, y: -event.scrollingDeltaY * scale))
        orbit.velocity = .zero
        applyOrbit(animated: false)
    }

    func magnified(_ event: NSEvent) {
        orbit.zoom = min(3, max(0.5, orbit.zoom * (1 + event.magnification)))
        frameCamera()
    }

    // MARK: Keys

    /// Typing goes to the simulator; command shortcuts stay with the menus.
    func keyDown(_ event: NSEvent) -> Bool {
        lastInputAt = Date()
        guard !event.modifierFlags.contains(.command), let hid,
              let usage = Keys.fromKeyCode[event.keyCode] else { return false }
        hid.key(usage, down: true)
        return true
    }

    func keyUp(_ event: NSEvent) -> Bool {
        guard !event.modifierFlags.contains(.command), let hid,
              let usage = Keys.fromKeyCode[event.keyCode] else { return false }
        hid.key(usage, down: false)
        return true
    }

    func flagsChanged(_ event: NSEvent) {
        guard let hid else { return }
        let flags = event.modifierFlags
        for (flag, usage) in Keys.modifiers where flags.contains(flag) != heldModifiers.contains(flag) {
            hid.key(usage, down: flags.contains(flag))
        }
        heldModifiers = flags.intersection([.control, .shift, .option])
    }

    @objc func pressHome() { hidAsync { $0.press(.home) } }
    @objc func pressLock() { hidAsync { $0.press(.lock) } }

    /// Pastes the clipboard's text by typing it.
    @objc func paste() {
        guard let text = NSPasteboard.general.string(forType: .string), !text.isEmpty else { return }
        hidAsync { $0.type(text) }
    }

    func hidAsync(_ work: @escaping (HID) -> Void) {
        guard let hid else { return }
        DispatchQueue.global(qos: .userInitiated).async { work(hid) }
    }

    // MARK: Starting the simulator

    func boot() {
        showStatus("Starting \(name)…", busy: true)
        let developerDir = developerDir, udid = udid
        DispatchQueue.global(qos: .userInitiated).async {
            let result = CoreSim.deviceSet(developerDir: developerDir).flatMap { set -> Result<(NSObject, NSObject), SimFailure> in
                guard let device = CoreSim.device(udid, in: set) else {
                    return .failure(SimFailure(message: "No simulator with id \(udid) is installed. Pick another with Leaf: Open Simulator…, or list them with `leaf sim list`."))
                }
                return .success((set, device))
            }
            DispatchQueue.main.async {
                switch result {
                case .failure(let failure): self.fail(failure)
                case .success(let (_, device)): self.started(device)
                }
            }
        }
    }

    func started(_ device: NSObject) {
        self.device = device
        let state = device.responds(to: NSSelectorFromString("stateString")) ? device.value(forKey: "stateString") as? String : nil
        let booted = state == "Booted"
        // Booted, but by a leaf pane that's gone: still leaf's to shut down.
        // The Duo is shut down with its pane however it was booted: it's
        // heavy, and nothing but leaf drives its cover.
        bootedHere = !booted || BootOwner.orphaned(ownerFile) || pose != nil
        if bootedHere { guardShutdown() }
        if !booted {
            showStatus("Booting \(name)…", busy: true)
            simctl(["boot", udid]) { [weak self] status, output in
                guard let self else { return }
                // "Unable to boot device in current state: Booted" is fine.
                if status != 0 && !output.contains("current state: Booted") {
                    self.bootedHere = false
                    if let owner = self.ownerFile { BootOwner.release(owner) }
                    self.fail(SimFailure(message: "\(self.name) didn't boot: \(Simulator.lastLine(output) ?? "simctl exited with \(status)"). Try `xcrun simctl boot \(self.udid)` in a terminal to see more."))
                    return
                }
                self.followBoot()
                self.attachDisplay(tries: 0)
            }
        } else {
            followBoot()
            attachDisplay(tries: 0)
        }
    }

    /// Shows what `simctl bootstatus` reports until the device is ready.
    func followBoot() {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        task.arguments = ["simctl", "bootstatus", udid]
        task.environment = simctlEnvironment()
        let pipe = Pipe()
        task.standardOutput = pipe
        task.standardError = pipe
        var pending = Data()
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            if chunk.isEmpty { handle.readabilityHandler = nil; return }
            pending.append(chunk)
            var latest: String?
            while let end = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let line = String(decoding: pending[pending.startIndex..<end], as: UTF8.self)
                pending.removeSubrange(pending.startIndex...end)
                if let step = Simulator.bootStep(line) { latest = step }
            }
            if let latest { DispatchQueue.main.async { self?.bootProgress(latest) } }
        }
        task.terminationHandler = { [weak self] finished in
            DispatchQueue.main.async { self?.bootFinished(finished.terminationStatus) }
        }
        do { try task.run() } catch {
            bootFinished(0)
        }
    }

    /// A `bootstatus` line worth showing, cleaned up; nil for noise.
    static func bootStep(_ line: String) -> String? {
        let text = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !text.hasPrefix("Monitoring boot status"), !text.contains("<private>") else { return nil }
        if text.hasPrefix("Waiting on ") { return String(text.dropFirst("Waiting on ".count)) }
        if text.hasPrefix("Device already booted") { return nil }
        if text.hasPrefix("Finished") { return nil }
        return text
    }

    func bootProgress(_ step: String) {
        guard !booted else { return }
        showStatus("Booting \(name): \(step)…", busy: true)
    }

    var booted = false

    func bootFinished(_ status: Int32) {
        booted = true
        if display == nil {
            showStatus("Connecting to the screen…", busy: true)
        } else {
            fadeStatus()
        }
    }

    /// Fades the boot progress out once the screen is live; a message that
    /// isn't progress (a failure) stays.
    func fadeStatus() {
        guard !spinner.isHidden else { return }
        NSAnimationContext.runAnimationGroup({ context in
            context.duration = 0.3
            status.animator().alphaValue = 0
        }, completionHandler: { [weak self] in
            guard let self, self.hid != nil else { return }
            self.status.isHidden = true
            self.status.alphaValue = 1
        })
    }

    /// Waits for the display to appear (it does early in the boot, so the
    /// boot animation shows), then puts it on the phone. The Duo has two:
    /// its cover, the smaller, and its inner screen; both are wanted (or
    /// after a few seconds, whichever came), and its hinge starts folded.
    func attachDisplay(tries: Int) {
        guard let device else { return }
        let screens = CoreSim.builtInDisplays(of: device).compactMap { display in
            CoreSim.surface(of: display).map { (display: display, surface: $0) }
        }
        let wanted = pose != nil && tries < 50 ? 2 : 1
        guard screens.count >= wanted,
              let (display, surface) = screens.min(by: { $0.surface.width * $0.surface.height < $1.surface.width * $1.surface.height }) else {
            if tries > 300 {
                fail(SimFailure(message: "\(name) booted, but its screen never appeared. Close this pane and open the simulator again; if it keeps happening, `xcrun simctl shutdown \(udid)` resets it."))
                return
            }
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [weak self] in self?.attachDisplay(tries: tries + 1) }
            return
        }
        self.display = display
        show(surface)
        if pose != nil, screens.count >= 2,
           let inner = screens.max(by: { $0.surface.width * $0.surface.height < $1.surface.width * $1.surface.height }) {
            duoDisplays = (cover: display, inner: inner.display)
        }
        var watched = true
        for watchedDisplay in duoDisplays.map({ [$0.cover, $0.inner] }) ?? [display] {
            watched = CoreSim.watchSurfaces(of: watchedDisplay) { [weak self] in
                DispatchQueue.main.async { self?.surfaceChanged() }
            } && watched
        }
        if !watched {
            Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in self?.surfaceChanged() }
        }
        if pose != nil {
            startHinge()
            // Opened before the screens were up: catch up.
            showDuoScreen()
        }
        switch HID.connect(to: device) {
        case .success(let hid):
            self.hid = hid
            if let shown = self.surface { routeTouches(to: shown) }
            startServer()
        case .failure(let failure):
            fail(failure)
        }
        updateControls()
        if booted { fadeStatus() }
    }

    func surfaceChanged() {
        guard let display, let latest = CoreSim.surface(of: display), latest !== surface else { return }
        show(latest)
        // A swapped surface can mean a restarted display server, with new IDs.
        routeTouches(to: latest)
    }

    /// Shows the Duo's screen that faces front: the inner one open, the
    /// cover folded.
    func showDuoScreen() {
        guard let duoDisplays, let pose else { return }
        let wanted = pose == .open ? duoDisplays.inner : duoDisplays.cover
        guard wanted !== display else { return }
        display = wanted
        if let latest = CoreSim.surface(of: wanted) { show(latest) }
    }

    /// Starts the hinge helper inside the Duo (src/duo-hinge.m), which tells
    /// iOS where the hinge is, and folds it to match the phone.
    func startHinge() {
        guard hingeInput == nil, let pose else { return }
        guard let helper = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("leaf-duo-hinge"),
              let size = (try? FileManager.default.attributesOfItem(atPath: helper.path))?[.size] as? Int, size > 0 else {
            fail(SimFailure(message: "This leaf was built without the iPhone Duo's hinge helper (the iOS Simulator SDK wasn't available), so \(name)'s iOS stays on its cover screen. Install Xcode's iOS platform, then reinstall leaf."))
            return
        }
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        task.arguments = ["simctl", "spawn", udid, helper.path]
        task.environment = simctlEnvironment()
        let input = Pipe(), errors = Pipe()
        task.standardInput = input
        task.standardOutput = FileHandle.nullDevice
        task.standardError = errors
        errors.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let text = String(decoding: handle.availableData, as: UTF8.self)
            guard let line = Simulator.lastLine(text) else { return }
            DispatchQueue.main.async { self?.fail(SimFailure(message: "\(line) Fold and open the phone again to retry.")) }
        }
        task.terminationHandler = { [weak self] _ in
            errors.fileHandleForReading.readabilityHandler = nil
            DispatchQueue.main.async { self?.hingeInput = nil }
        }
        do { try task.run() } catch {
            fail(SimFailure(message: "Couldn't start the iPhone Duo's hinge helper (\(error.localizedDescription)), so \(name)'s iOS stays on its cover screen. Close the pane and open it again."))
            return
        }
        // Writing to a helper that has exited must fail the write, not kill the pane.
        signal(SIGPIPE, SIG_IGN)
        hingeInput = input.fileHandleForWriting
        moveHinge(to: pose == .open ? 180 : 0, settled: true)
    }

    /// Tells iOS the hinge is at `degrees` (0 shut, 180 flat). Mid-swing
    /// steps smaller than a few degrees are skipped; a settled angle is
    /// always sent.
    func moveHinge(to degrees: Double, settled: Bool) {
        if hingeInput == nil, !settled { return }
        if hingeInput == nil { startHinge() }
        guard let hingeInput else { return }
        if !settled, let last = lastHingeAngle, abs(degrees - last) < 4 { return }
        lastHingeAngle = degrees
        // A helper that has exited fails the write; its termination handler
        // clears `hingeInput`, and the next settled angle starts it again.
        try? hingeInput.write(contentsOf: Data(String(format: "%.1f\n", degrees).utf8))
    }

    /// Points the Duo's touches at the screen leaf shows, found in `simctl io
    /// enumerate` by its pixel size. Other iPhones keep the built-in
    /// digitizer. Until this resolves, Duo touches go nowhere.
    func routeTouches(to surface: IOSurface) {
        guard pose != nil else { return }
        let size = (surface.width, surface.height)
        simctl(["io", udid, "enumerate"]) { [weak self] status, output in
            guard let self, let hid = self.hid else { return }
            // The phone folded or opened meanwhile; that change routes again.
            guard let shown = self.surface, (shown.width, shown.height) == size else { return }
            guard status == 0, let screen = Simulator.screen(in: output, width: size.0, height: size.1) else {
                self.fail(SimFailure(message: "\(self.name)'s screen shows, but touches can't reach it: leaf couldn't find its \(size.0)×\(size.1) screen in `xcrun simctl io \(self.udid) enumerate`\(status == 0 ? "" : " (simctl exited with \(status))"). Close the pane and open it again."))
                return
            }
            hid.route(to: screen)
        }
    }

    /// The connected screen `width`×`height` pixels in `simctl io enumerate`
    /// output, whose entries read "Screen ID: 1", "Unique ID: <uuid>",
    /// "Pixel Size: {1398, 2034}".
    static func screen(in enumeration: String, width: Int, height: Int) -> HID.TouchScreen? {
        var id: UInt32?, uniqueID: String?
        for line in enumeration.split(whereSeparator: \.isNewline) {
            let field = line.trimmingCharacters(in: .whitespaces)
            func labeled(_ label: String) -> String? {
                field.hasPrefix(label) ? String(field.dropFirst(label.count)) : nil
            }
            if let value = labeled("Screen ID: ") {
                id = UInt32(value)
                uniqueID = nil
            } else if let value = labeled("Unique ID: ") {
                uniqueID = value
            } else if let value = labeled("Pixel Size: "), let id, let uniqueID {
                let numbers = value.split(whereSeparator: { !$0.isNumber }).compactMap { Int($0) }
                if numbers == [width, height] { return .screen(id: id, uniqueID: uniqueID) }
            }
        }
        return nil
    }

    /// Puts `surface` on the phone's screen as a texture over the same memory.
    func show(_ surface: IOSurface) {
        guard let phone, let device = phoneView.device ?? MTLCreateSystemDefaultDevice() else { return }
        let format: MTLPixelFormat
        switch IOSurfaceGetPixelFormat(surface) {
        case 0x52474241: format = .rgba8Unorm_srgb // 'RGBA'
        case 0x6c313072: format = .bgr10a2Unorm // 'l10r'
        default: format = .bgra8Unorm_srgb // 'BGRA'
        }
        let descriptor = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: format, width: surface.width,
                                                                  height: surface.height, mipmapped: false)
        descriptor.usage = .shaderRead
        descriptor.storageMode = device.hasUnifiedMemory ? .shared : .managed
        guard let texture = device.makeTexture(descriptor: descriptor, iosurface: surface, plane: 0) else {
            fail(SimFailure(message: "The simulator's screen couldn't be drawn here (Metal refused its \(surface.width)×\(surface.height) surface). Close this pane and try again."))
            return
        }
        self.surface = surface
        phone.pivot.removeAction(forKey: "loading")
        phone.material.diffuse.contents = texture
        dressGlass()
    }

    // MARK: Agent control

    func startServer() {
        guard server == nil, let path = ProcessInfo.processInfo.environment["LEAF_SIM_SOCKET"], !path.isEmpty else { return }
        let control = ControlServer(path: path) { [weak self] request in
            self?.handle(request) ?? ["failed": ["message": "The simulator pane is closing."]]
        }
        switch control.start() {
        case .success:
            server = control
        case .failure(let failure):
            // The pane still works by hand; only the agent can't drive it.
            showStatus(failure.message, busy: false)
        }
    }

    /// One request from `leaf sim`, on the server's thread. Coordinates are
    /// screenshot pixels, as `leaf sim screenshot` saves them.
    func handle(_ request: [String: Any]) -> [String: Any] {
        guard let (hid, width, height) = DispatchQueue.main.sync(execute: { () -> (HID, Int, Int)? in
            guard let hid = self.hid, let surface = self.surface else { return nil }
            return (hid, surface.width, surface.height)
        }) else {
            return ["failed": ["message": "\(name) is still starting. Wait for its home screen, then try again."]]
        }
        func fraction(_ value: Any?, _ size: Int) -> CGFloat? {
            guard let number = value as? NSNumber else { return nil }
            return CGFloat(truncating: number) / CGFloat(size)
        }
        func point(_ value: Any?) -> CGPoint? {
            guard let pair = value as? [Any], pair.count == 2,
                  let x = fraction(pair[0], width), let y = fraction(pair[1], height),
                  (0...1).contains(x), (0...1).contains(y) else { return nil }
            return CGPoint(x: x, y: y)
        }
        let outside = "is outside the screen, which is \(width)×\(height) pixels with 0,0 at the top left"
        if let tap = request["tap"] as? [String: Any] {
            guard let at = point(tap["at"]) else { return failed("The tap \(outside).") }
            ripple(at)
            hid.tap(at: at)
            return ["ok": [:]]
        }
        if let swipe = request["swipe"] as? [String: Any] {
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
            return failed("Typed the rest, but these characters aren't on a US keyboard and were skipped: \(list). Paste such text with `xcrun simctl pbcopy \(udid)` and a long-press Paste.")
        }
        if let key = request["key"] as? [String: Any], let name = key["name"] as? String {
            guard let usage = Keys.named[name.lowercased()] else {
                return failed("No key named \(name). Keys: \(Keys.named.keys.sorted().joined(separator: ", ")).")
            }
            hid.key(usage, down: true)
            usleep(40_000)
            hid.key(usage, down: false)
            hid.queue.sync {}
            return ["ok": [:]]
        }
        if let button = request["button"] as? [String: Any], let name = button["name"] as? String {
            guard let which = HID.Button(rawValue: name.lowercased()) else {
                return failed("No button named \(name). Buttons: \(HID.Button.allCases.map(\.rawValue).joined(separator: ", ")).")
            }
            hid.press(which)
            return ["ok": [:]]
        }
        if request["info"] != nil {
            return ["ok": ["udid": udid, "name": name, "runtime": runtime, "width": width, "height": height]]
        }
        return failed("leaf's simulator pane didn't understand that request. Update leaf so the CLI and the pane match.")
    }

    func failed(_ message: String) -> [String: Any] { ["failed": ["message": message]] }

    /// A ring where the agent touches, so the person sees what it does.
    func ripple(_ at: CGPoint) {
        DispatchQueue.main.async { [weak self] in
            guard let self, let phone = self.phone, let glass = self.glass else { return }
            let fit = self.picture
            let x = fit.minX + at.x * fit.width, y = fit.minY + at.y * fit.height
            let ring = SCNNode(geometry: SCNTorus(ringRadius: 0.035, pipeRadius: 0.004))
            let material = SCNMaterial()
            material.lightingModel = .constant
            material.diffuse.contents = NSColor.white.withAlphaComponent(0.9)
            material.readsFromDepthBuffer = false
            ring.geometry?.materials = [material]
            ring.renderingOrder = 10
            ring.simdPosition = SIMD3(Float((x - 0.5) * glass.size.width), Float((0.5 - y) * glass.size.height), 0.002)
            ring.simdEulerAngles = SIMD3(Float.pi / 2, 0, 0)
            phone.pivot.addChildNode(ring)
            ring.runAction(.sequence([
                .group([.scale(to: 1.8, duration: 0.45), .fadeOut(duration: 0.45)]),
                .removeFromParentNode(),
            ]))
        }
    }

    // MARK: Asking the agent

    @objc func toggleAsk() {
        if askShown { closeAsk(); return }
        askShown = true
        marking = true
        mark = nil
        if !orbit.front { toggleFront() }
        askCard.isHidden = false
        askCard.field.stringValue = ""
        updateAskNote()
        window.invalidateCursorRects(for: phoneView)
        layout()
        updateControls()
        window.makeFirstResponder(askCard.field)
        askWhichAgent()
    }

    func closeAsk() {
        askShown = false
        marking = false
        asking = false
        mark = nil
        marquee.isHidden = true
        askCard.isHidden = true
        window.invalidateCursorRects(for: phoneView)
        layout()
        updateControls()
        window.makeFirstResponder(phoneView)
    }

    func updateAskNote() {
        askCard.note.textColor = .secondaryLabelColor
        askCard.note.stringValue = mark == nil
            ? "Drag on the screen to mark part of it, or ask about the whole screen."
            : "Asking about the marked part. Drag again to change it."
    }

    func nameAgent() {
        ask.toolTip = "Ask \(agentName) about the screen (⇧⌘A)"
        askCard.field.placeholderString = "Ask \(agentName) about this screen…"
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        if selector == #selector(NSResponder.cancelOperation(_:)) { closeAsk(); return true }
        return false
    }

    /// Hands the question to leaf, which saves a screenshot and sends both.
    @objc func sendAsk() {
        guard !asking else { return }
        var request: [String: Any] = ["question": askCard.field.stringValue]
        if let mark, let surface {
            let w = CGFloat(surface.width), h = CGFloat(surface.height)
            request["region"] = [Int(mark.minX * w), Int(mark.minY * h), Int(mark.width * w), Int(mark.height * h)]
        }
        guard tellLeaf(["ask": request]) else {
            askCard.note.stringValue = "leaf stopped listening, so nothing was sent. Close this pane and open the simulator again."
            askCard.note.textColor = .systemRed
            return
        }
        asking = true
        askCard.note.textColor = .secondaryLabelColor
        askCard.note.stringValue = "Sending to \(agentName)…"
    }

    func tellLeaf(_ request: [String: Any]) -> Bool {
        guard let data = try? JSONSerialization.data(withJSONObject: request) else { return false }
        do {
            try FileHandle.standardOutput.write(contentsOf: data + Data("\n".utf8))
            return true
        } catch {
            return false
        }
    }

    func askWhichAgent() { _ = tellLeaf(["agent": [String: Any]()]) }

    /// leaf's answer to a question, one JSON line.
    func replied(_ line: Data) {
        guard let object = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] else { return }
        if let agent = object["agent"] as? [String: Any] {
            let name = agent["name"] as? String ?? ""
            agentName = name.isEmpty ? "your agent" : name
            nameAgent()
            return
        }
        guard asking else { return }
        asking = false
        if let sent = object["sent"] as? [String: Any] {
            let agent = sent["agent"] as? String ?? agentName
            agentName = agent
            nameAgent()
            askCard.note.stringValue = "Sent to \(agent). Its answer is in the \(agent) pane."
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.6) { [weak self] in
                guard let self, self.askShown, !self.asking else { return }
                self.closeAsk()
            }
        } else if let failed = object["failed"] as? [String: Any] {
            var message = failed["message"] as? String ?? "Sending failed for a reason leaf didn't give."
            if let prompt = failed["prompt"] as? String, !prompt.isEmpty {
                NSPasteboard.general.clearContents()
                if NSPasteboard.general.setString(prompt, forType: .string) {
                    message += " The question was copied to the clipboard."
                }
            }
            askCard.note.stringValue = message
            askCard.note.textColor = .systemRed
        }
    }

    func listenToLeaf() {
        var pending = Data()
        FileHandle.standardInput.readabilityHandler = { [weak self] handle in
            let chunk = handle.availableData
            if chunk.isEmpty { handle.readabilityHandler = nil; return }
            pending.append(chunk)
            while let end = pending.firstIndex(of: UInt8(ascii: "\n")) {
                let line = Data(pending[pending.startIndex..<end])
                pending.removeSubrange(pending.startIndex...end)
                DispatchQueue.main.async { self?.replied(line) }
            }
        }
    }

    // MARK: simctl

    func simctlEnvironment() -> [String: String] {
        var environment = ProcessInfo.processInfo.environment
        environment["DEVELOPER_DIR"] = developerDir
        return environment
    }

    /// Runs `xcrun simctl …` off the main thread; `done` gets its status and
    /// output on the main thread.
    func simctl(_ arguments: [String], done: @escaping (Int32, String) -> Void) {
        let environment = simctlEnvironment()
        DispatchQueue.global(qos: .userInitiated).async {
            let task = Process()
            task.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
            task.arguments = ["simctl"] + arguments
            task.environment = environment
            let pipe = Pipe()
            task.standardOutput = pipe
            task.standardError = pipe
            var status: Int32 = -1
            var output = ""
            do {
                try task.run()
                let data = pipe.fileHandleForReading.readDataToEndOfFile()
                task.waitUntilExit()
                status = task.terminationStatus
                output = String(decoding: data, as: UTF8.self)
            } catch {
                output = "couldn't run xcrun (\(error.localizedDescription))"
            }
            DispatchQueue.main.async { done(status, output) }
        }
    }

    static func lastLine(_ text: String) -> String? {
        text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.last { !$0.isEmpty }
    }

    // MARK: Lifecycle

    override func start() {
        super.start()
        Timer.scheduledTimer(withTimeInterval: 1.0 / 60.0, repeats: true) { [weak self] _ in self?.animate() }
            .tolerance = 0.002
        listenToLeaf()
        askWhichAgent()
        loadPhone()
        boot()
    }

    /// A device leaf booted is shut down by its guardian once this helper
    /// exits (see `guardShutdown`); one that was already running is left
    /// running.
    override func willShutDown() {
        server?.stop()
    }

    /// Records this helper as the device's owner, next to the control
    /// socket. Nil without a socket path.
    var ownerFile: String? {
        guard let socket = ProcessInfo.processInfo.environment["LEAF_SIM_SOCKET"], !socket.isEmpty else { return nil }
        return socket + ".owner"
    }

    /// Starts a guardian that shuts the device down once this helper exits,
    /// however it goes. Closing the pane hangs up its processes and quitting
    /// Rex can kill them outright, so the helper can't count on running its
    /// own cleanup. The guardian runs in its own session, out of reach of
    /// that hangup, and stands down if another pane has taken the device.
    func guardShutdown() {
        guard let owner = ownerFile else { return }
        let pid = getpid()
        guard BootOwner.claim(owner, pid: pid) else { return }
        let script = """
        while kill -0 "$1" 2>/dev/null; do sleep 0.5; done
        [ "$(cat "$3" 2>/dev/null)" = "$1" ] || exit 0
        rm -f "$3"
        exec /usr/bin/xcrun simctl shutdown "$2"
        """
        BootOwner.spawnDetached(["/bin/sh", "-c", script, "leaf-sim-guardian", "\(pid)", udid, owner],
                                environment: simctlEnvironment())
    }
}

// MARK: - Boot ownership

/// Which helper booted a device: a file holding that helper's pid, so the
/// device is shut down when it goes, even after a crash.
enum BootOwner {
    /// True when `file` names a helper that has exited: leaf booted the
    /// device, and nothing shut it down.
    static func orphaned(_ file: String?) -> Bool {
        guard let file, let text = try? String(contentsOfFile: file, encoding: .utf8),
              let pid = pid_t(text.trimmingCharacters(in: .whitespacesAndNewlines)) else { return false }
        return kill(pid, 0) != 0 && errno == ESRCH
    }

    static func claim(_ file: String, pid: pid_t) -> Bool {
        (try? "\(pid)".write(toFile: file, atomically: true, encoding: .utf8)) != nil
    }

    static func release(_ file: String) {
        try? FileManager.default.removeItem(atPath: file)
    }

    /// Starts `arguments` in a new session with no open files but /dev/null,
    /// so neither the pane's hangup nor its pipes tie it to this process.
    @discardableResult
    static func spawnDetached(_ arguments: [String], environment: [String: String]) -> Bool {
        var attributes: posix_spawnattr_t?
        posix_spawnattr_init(&attributes)
        defer { posix_spawnattr_destroy(&attributes) }
        posix_spawnattr_setflags(&attributes, Int16(POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT))
        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        defer { posix_spawn_file_actions_destroy(&actions) }
        for fd: Int32 in 0...2 {
            posix_spawn_file_actions_addopen(&actions, fd, "/dev/null", fd == 0 ? O_RDONLY : O_WRONLY, 0)
        }
        let argv = arguments.map { strdup($0) } + [nil]
        let envp = environment.map { strdup("\($0.key)=\($0.value)") } + [nil]
        defer { (argv + envp).forEach { free($0) } }
        var pid: pid_t = 0
        return posix_spawn(&pid, arguments[0], &actions, &attributes, argv, envp) == 0
    }
}

// MARK: - Control socket

/// A Unix socket that takes one JSON line per connection and answers with
/// one. Only this user can connect (the socket is mode 0600).
final class ControlServer {
    let path: String
    let handle: ([String: Any]) -> [String: Any]
    private var listener: Int32 = -1

    init(path: String, handle: @escaping ([String: Any]) -> [String: Any]) {
        self.path = path
        self.handle = handle
    }

    func start() -> Result<Void, SimFailure> {
        let busy = SimFailure(message: "This simulator is already open in another Leaf pane, so the agent drives that one. Close the other pane to drive this one.")
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8) + [0]
        guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            return .failure(SimFailure(message: "The agent can't drive this simulator: the socket path \(path) is longer than macOS allows."))
        }
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: bytes) }
        address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        // A live socket at the path belongs to another pane on this device.
        if ControlServer.answers(path) { return .failure(busy) }
        unlink(path)
        try? FileManager.default.createDirectory(atPath: (path as NSString).deletingLastPathComponent,
                                                 withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return .failure(SimFailure(message: "The agent can't drive this simulator: no socket (\(String(cString: strerror(errno))))).")) }
        let bound = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        }
        guard bound == 0, chmod(path, 0o600) == 0, listen(fd, 8) == 0 else {
            let why = String(cString: strerror(errno))
            close(fd)
            return .failure(SimFailure(message: "The agent can't drive this simulator: \(path) couldn't be opened (\(why))."))
        }
        listener = fd
        let thread = Thread { [weak self] in self?.serve(fd) }
        thread.name = "leaf.sim.control"
        thread.start()
        return .success(())
    }

    func stop() {
        if listener >= 0 { close(listener) }
        unlink(path)
    }

    static func answers(_ path: String) -> Bool {
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8) + [0]
        guard bytes.count <= MemoryLayout.size(ofValue: address.sun_path) else { return false }
        withUnsafeMutableBytes(of: &address.sun_path) { $0.copyBytes(from: bytes) }
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        return withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) }
        } == 0
    }

    private func serve(_ fd: Int32) {
        while true {
            let client = accept(fd, nil, nil)
            if client < 0 {
                if errno == EINTR { continue }
                return
            }
            var one: Int32 = 1
            setsockopt(client, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
            let reply: [String: Any]
            if let line = ControlServer.readLine(client),
               let request = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] {
                reply = handle(request)
            } else {
                reply = ["failed": ["message": "leaf's simulator pane got a request it couldn't read."]]
            }
            if var data = try? JSONSerialization.data(withJSONObject: reply) {
                data.append(UInt8(ascii: "\n"))
                data.withUnsafeBytes { raw in
                    var sent = 0
                    while sent < raw.count {
                        let wrote = write(client, raw.baseAddress?.advanced(by: sent), raw.count - sent)
                        if wrote <= 0 { break }
                        sent += wrote
                    }
                }
            }
            close(client)
        }
    }

    /// One line, at most 1 MB, without its newline.
    private static func readLine(_ fd: Int32) -> Data? {
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while data.count < 1 << 20 {
            let count = read(fd, &buffer, buffer.count)
            if count <= 0 { break }
            if let end = buffer[0..<count].firstIndex(of: UInt8(ascii: "\n")) {
                data.append(contentsOf: buffer[0..<end])
                return data
            }
            data.append(contentsOf: buffer[0..<count])
        }
        return data.isEmpty ? nil : data
    }
}

// MARK: - App

final class SimulatorDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    let simulator: Simulator

    init(simulator: Simulator) {
        self.simulator = simulator
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = menu()
        simulator.window.delegate = self
        NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self else { return event }
            if event.window === self.simulator.window { self.simulator.lastInputAt = Date() }
            return self.simulator.handleRexKey(event) ? nil : event
        }
        simulator.start()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { self.simulator.takeFocus() }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    func windowDidBecomeKey(_ notification: Notification) { simulator.windowBecameKey() }

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
        let s = simulator
        submenu("Leaf", [
            item("Close Pane", #selector(Simulator.closePane), "w", target: s),
            item("Quit", #selector(Simulator.closePane), "q", target: s),
        ])
        submenu("Edit", [
            item("Cut", #selector(NSText.cut(_:)), "x"),
            item("Copy", #selector(NSText.copy(_:)), "c"),
            item("Paste", #selector(Simulator.paste), "v", target: s),
            item("Select All", #selector(NSText.selectAll(_:)), "a"),
            .separator(),
            item("Ask About the Screen", #selector(Simulator.toggleAsk), "a", [.command, .shift], target: s),
        ])
        submenu("Device", [
            item("Open", #selector(Simulator.toggleFold), "f", target: s),
            .separator(),
            item("Home", #selector(Simulator.pressHome), "h", [.command, .shift], target: s),
            item("Lock", #selector(Simulator.pressLock), "l", target: s),
        ])
        submenu("View", [
            item("Face Front / Turn in 3D", #selector(Simulator.toggleFront), "0", target: s),
            item("Zoom In", #selector(Simulator.zoomIn), "=", target: s),
            item("Zoom In", #selector(Simulator.zoomIn), "+", target: s),
            item("Zoom Out", #selector(Simulator.zoomOut), "-", target: s),
        ])
        return main
    }
}

@main
enum SimulatorMain {
    static func main() {
        let (session, block) = PaneApp.rexBlock(for: "the simulator")
        let environment = ProcessInfo.processInfo.environment
        guard let udid = environment["LEAF_SIM_UDID"], !udid.isEmpty,
              let developerDir = environment["LEAF_DEVELOPER_DIR"], !developerDir.isEmpty else {
            FileHandle.standardError.write(Data("leaf: the simulator pane needs LEAF_SIM_UDID and LEAF_DEVELOPER_DIR; open it with `leaf sim open`.\n".utf8))
            exit(1)
        }
        let simulator = Simulator(session: session, block: block, udid: udid,
                                  name: environment["LEAF_SIM_NAME"] ?? "Simulator",
                                  runtime: environment["LEAF_SIM_RUNTIME"] ?? "", developerDir: developerDir)
        PaneApp.run(SimulatorDelegate(simulator: simulator))
    }
}
