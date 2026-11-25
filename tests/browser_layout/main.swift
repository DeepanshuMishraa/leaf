// On macOS: xcrun swiftc src/browser.swift tests/browser_layout/main.swift -o /tmp/leaf-layout && /tmp/leaf-layout
import AppKit

typealias Object = [String: Any]

// Run this test executable as a stand-in for Rex so we can observe what actually gets sent.
let args = CommandLine.arguments
if args.dropFirst().starts(with: ["api", "call"]) {
    if args.count == 5, args[3] == "session.resize_layer",
       let log = ProcessInfo.processInfo.environment["LEAF_TEST_RESIZE_LOG"] {
        let handle = try FileHandle(forWritingTo: URL(fileURLWithPath: log))
        try handle.seekToEnd()
        try handle.write(contentsOf: Data((args[4] + "\n").utf8))
        try handle.close()
    }
    exit(0)
}
var failures = [String]()
func checkResize(_ condition: Bool, _ message: String) {
    if !condition { failures.append(message) }
}
let full = [0.0, 0, 1, 1]
let card = [0.125, 0.25, 0.5, 0.5]
func layer(_ kind: String, _ rect: [Double], _ id: String) -> Object {
    ["kind": kind, "layer_id": "layer-\(id)", "bounds": rect, "layout": ["rect": rect, "block_id": id] as Object]
}
func view(_ layers: [Object], zoom: String = "", active: String = "w") -> Object {
    ["active_window_id": active, "windows": [["window_id": "w", "layers": layers,
        "zoomed_block_id": zoom, "focused_block_id": "web"] as Object]]
}
let chrome = Chrome(top: 8, left: 8, right: 323, bottom: 8, gap: 8, header: 36, radius: 10, floatTop: 42)
let host = CGRect(x: 100, y: 50, width: 1931, height: 1050)
func frame(_ snapshot: Object, _ id: String = "web") -> CGRect? {
    SessionView.placement(snapshot, block: id).flatMap { chrome.body($0, in: host) }
}
let tile = layer("tiled", full, "base")
var floating = layer("floating", card, "web")
// Float frame: (308, 342) to (1108, 842). Paint to the 1pt border, but yield input in the outer 8pt.
let whole = frame(view([tile, floating]))!
let placed = SessionView.placement(view([tile, floating]), block: "web")!
precondition(whole == CGRect(x: 309, y: 378, width: 798, height: 463))
for point in [CGPoint(x: 312, y: 590), CGPoint(x: 1104, y: 590), CGPoint(x: 700, y: 838), CGPoint(x: 1104, y: 838)] {
    precondition(whole.contains(point) && chrome.resizeGrip(at: point, placement: placed, in: host) != nil)
}
precondition(chrome.resizeGrip(at: CGPoint(x: 1098, y: 830), placement: placed, in: host) == nil)
let corner = chrome.resizeGrip(at: CGPoint(x: 1104, y: 838), placement: placed, in: host)!
precondition(corner.resized(dx: 200, dy: 125) == CGRect(x: 0.125, y: 0.25, width: 0.625, height: 0.625))
precondition(corner.resized(dx: 4000, dy: 4000) == CGRect(x: 0.125, y: 0.25, width: 0.875, height: 0.75))
precondition(frame(view([tile, floating]), "base") == nil)

// A narrow float still uses the same visible border and minimum size.
func smallFloat(_ w: Double, _ h: Double) -> CGRect? {
    frame(view([tile, layer("floating", [0.125, 0.25, w / 1600, h / 1000], "web")]))
}
precondition(smallFloat(96, 104)?.size == CGSize(width: 94, height: 67))
precondition(smallFloat(83, 98) != nil)  // original body 81 × 61
precondition(smallFloat(82, 104) == nil) // original width exactly 80
precondition(smallFloat(96, 97) == nil)  // original height exactly 60

floating["layout"] = ["rect": card, "split": ["direction": "horizontal",
    "before": ["block_id": "left", "rect": [0.125, 0.25, 0.25, 0.5]] as Object,
    "after": ["block_id": "web", "rect": [0.375, 0.25, 0.25, 0.5]] as Object] as Object] as Object
// Both children keep the same outer frame, not a second grip at their shared divider.
precondition(frame(view([tile, floating]), "left") == CGRect(x: 309, y: 378, width: 394, height: 463))
let right = CGRect(x: 713, y: 378, width: 394, height: 463)
precondition(frame(view([tile, floating])) == right)
let splitChild = SessionView.placement(view([tile, floating]), block: "web")!
precondition(chrome.resizeGrip(at: CGPoint(x: right.minX + 2, y: right.midY), placement: splitChild, in: host) == nil)
precondition(frame(view([tile, floating, layer("floating", [0.625, 0.25, 0.25, 0.5], "edge")])) == right)
let cover = layer("floating", [0.5, 0.25, 0.25, 0.5], "upper")
precondition(frame(view([tile, floating, cover])) == nil)
precondition(frame(view([tile, cover, floating])) == right)
precondition(frame(view([tile, floating], zoom: "web")) == CGRect(x: 109, y: 94, width: 1598, height: 997))
precondition(frame(view([tile, floating], zoom: "base")) == nil)
let otherTab = view([tile, floating], active: "other")
precondition(frame(otherTab) == nil && SessionView.contains(otherTab, block: "web"))
floating["state"] = "minimized"
let minimized = view([tile, floating])
precondition(frame(minimized) == nil && SessionView.contains(minimized, block: "web"))
precondition(frame(minimized, "base") == frame(view([tile]), "base"))

// The float's lower edge and tiled split share a fraction, but not a screen coordinate.
var splitTile = tile
splitTile["layout"] = ["rect": full, "split": ["direction": "vertical",
    "before": ["block_id": "top", "rect": [0, 0, 1, 0.5]] as Object,
    "after": ["block_id": "web", "rect": [0, 0.5, 1, 0.5]] as Object] as Object] as Object
var noHeaders = chrome
noHeaders.header = 0
let shortHost = CGRect(x: 0, y: 0, width: 1048, height: 616)
let lower = SessionView.placement(view([splitTile]), block: "web")!
precondition(noHeaders.body(lower, in: shortHost)?.minY == 313)
let upper = layer("floating", [0, 0, 1, 0.5], "upper")
let covered = SessionView.placement(view([splitTile, upper]), block: "web")!
precondition(noHeaders.body(covered, in: shortHost) == nil)

// Shrink a two-direction split from either corner. Only the grabbed child's body must stay visible.
func splitCorner(_ r: CGRect, first: Bool) -> Object {
    func rect(_ r: CGRect) -> [Double] { [Double(r.minX), Double(r.minY), Double(r.width), Double(r.height)] }
    func leaf(_ r: CGRect, _ id: String) -> Object { ["rect": rect(r), "block_id": id] }
    let (column, other) = r.divided(atDistance: r.width / 2, from: first ? .minXEdge : .maxXEdge)
    let (target, sibling) = column.divided(atDistance: column.height / 4, from: first ? .minYEdge : .maxYEdge)
    let row: Object = ["rect": rect(column), "split": ["direction": "vertical",
        "before": first ? leaf(target, "web") : leaf(sibling, "other-row"),
        "after": first ? leaf(sibling, "other-row") : leaf(target, "web")] as Object]
    var result = layer("floating", rect(r), "web")
    result["layout"] = ["rect": rect(r), "split": ["direction": "horizontal",
        "before": first ? row : leaf(other, "other-col"), "after": first ? leaf(other, "other-col") : row] as Object] as Object
    return view([tile, result])
}
for first in [true, false] {
    let child = SessionView.placement(splitCorner(placed.rect, first: first), block: "web")!
    let outer = chrome.floatingFrame(placed.rect, in: host)
    let point = first ? CGPoint(x: outer.minX + 2, y: outer.minY + 2) : CGPoint(x: outer.maxX - 2, y: outer.maxY - 2)
    let grip = chrome.resizeGrip(at: point, placement: child, in: host)!
    let delta: CGFloat = first ? 900 : -900
    let clamped = grip.resized(dx: delta, dy: delta)
    precondition(clamped.width < placed.rect.width && clamped.height < placed.rect.height)
    precondition(frame(splitCorner(clamped, first: first)) != nil, "a valid drag hid its own split child")
}

// On a canvas starting at -650, the clamped child's two edges can straddle half-point ties.
let offsetHost = CGRect(x: -658, y: 0, width: 1331, height: 1050)
let offsetLayer = CGRect(x: 0.5625, y: 0.25, width: 0.25, height: 0.5)
let offsetChild = SessionView.placement(splitCorner(offsetLayer, first: false), block: "web")!
precondition(chrome.body(offsetChild, in: offsetHost) != nil)
let offsetGrip = chrome.resizeGrip(at: CGPoint(x: 160, y: 750), placement: offsetChild, in: offsetHost)!
let offsetClamped = offsetGrip.resized(dx: -1000, dy: 0)
let roundedChild = SessionView.placement(splitCorner(offsetClamped, first: false), block: "web")!
precondition(offsetClamped.width < offsetLayer.width && chrome.body(roundedChild, in: offsetHost) != nil,
             "rounding the clamped edges hid the split child")

// Keep the first request in flight on the main queue until the desired sequence of input events is complete.
enum DragSequence: CaseIterable { case switchAfterRelease, cancelSecond, finishSecond, continueSecond, differentLayer }
func checkResizeDelivery(_ sequence: DragSequence) throws {
    let log = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try Data().write(to: log)
    defer { try? FileManager.default.removeItem(at: log) }
    setenv("LEAF_REX", URL(fileURLWithPath: args[0]).standardizedFileURL.path, 1)
    setenv("LEAF_TEST_RESIZE_LOG", log.path, 1)
    _ = NSApplication.shared
    let browser = Browser(session: "fixture", block: "web")
    browser.placement = placed
    browser.shown = true
    func mouse(_ type: NSEvent.EventType, _ x: CGFloat) -> NSEvent {
        NSEvent.mouseEvent(with: type, location: CGPoint(x: x, y: 20), modifierFlags: [], timestamp: 0,
                           windowNumber: browser.window.windowNumber, context: nil, eventNumber: 1,
                           clickCount: 1, pressure: type == .leftMouseUp ? 0 : 1)!
    }
    let down = mouse(.leftMouseDown, 20)
    browser.resizeStart = (corner, browser.window.convertPoint(toScreen: down.locationInWindow), down)
    precondition(browser.handleEdgeEvent(mouse(.leftMouseDragged, 40)))
    precondition(browser.handleEdgeEvent(mouse(.leftMouseUp, 90)))
    let final = corner.resized(dx: 70, dy: 0)
    browser.apply(view([tile, layer("floating", [0.125, 0.25, 0.5125, 0.5], "web")]))
    precondition(browser.shown, "an older layout hid the pane before mouse-up was delivered")
    func drain() {
        let deadline = Date().addingTimeInterval(5)
        while browser.resizeInFlight != nil && Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.01))
        }
        precondition(browser.resizeInFlight == nil)
    }
    var expected = [(corner.id, corner.resized(dx: 20, dy: 0)), (corner.id, final)]
    if sequence != .switchAfterRelease {
        let grip = sequence == .differentLayer ? ResizeGrip(id: "new-layer", bounds: final, canvas: corner.canvas,
                       minimum: corner.minimum, horizontal: corner.horizontal, vertical: corner.vertical) : corner
        browser.resizeStart = (grip, browser.window.convertPoint(toScreen: down.locationInWindow), down)
        precondition(browser.handleEdgeEvent(mouse(.leftMouseDragged, 60)))
        if sequence == .continueSecond {
            drain()
            expected.append((grip.id, grip.resized(dx: 40, dy: 0)))
        }
        if sequence != .cancelSecond {
            precondition(browser.handleEdgeEvent(mouse(.leftMouseUp, 120)))
            if sequence == .finishSecond { expected.removeLast() } // newest completed size for this layer wins
            expected.append((grip.id, grip.resized(dx: 100, dy: 0)))
        }
    }
    browser.shownSession = "another-session"
    browser.follow()
    precondition(!browser.shown)
    drain()
    let requests = try String(contentsOf: log, encoding: .utf8).split(separator: "\n").map {
        try JSONSerialization.jsonObject(with: Data($0.utf8)) as! Object
    }
    checkResize(requests.count == expected.count && zip(requests, expected).allSatisfy { request, target in
        request["session_id"] as? String == "fixture" && request["layer_id"] as? String == target.0 &&
            SessionView.rect(request["bounds"]) == target.1
    }, "incorrect resize delivery for \(sequence)")
}
for sequence in DragSequence.allCases { try checkResizeDelivery(sequence) }

// The same snapshots reach both helpers, including after the sender's async resize has completed.
let initialLayout = splitCorner(placed.rect, first: true)
let resizedBounds = corner.resized(dx: 200, dy: 125)
let resizedLayout = splitCorner(resizedBounds, first: true)
for block in ["web", "other-col"] {
    let browser = Browser(session: "fixture", block: block)
    browser.wasFocusedInRex = block == "web"
    browser.placement = SessionView.placement(initialLayout, block: block)
    browser.shown = true
    browser.apply(resizedLayout)
    checkResize(browser.shown && browser.settleUntil < Date(), "layer resize hid \(block)")
    // A different split within the same outer bounds is still a layout transition.
    browser.apply(splitCorner(resizedBounds, first: false))
    precondition(!browser.shown && browser.settleUntil > Date())
    // Coalesced snapshots may change both the layer bounds and the split at once.
    browser.shown = true
    browser.settleUntil = .distantPast
    browser.apply(initialLayout)
    precondition(!browser.shown && browser.settleUntil > Date())
}

if !failures.isEmpty {
    FileHandle.standardError.write(Data((failures.joined(separator: "\n") + "\n").utf8))
    exit(1)
}

// A different session in Rex's sidebar doesn't change this session's focused block.
let selected = #"{"sessionID":"fixture"}"#
precondition(Chrome.sessionID(from: Data(selected.utf8)) == "fixture")
precondition(Chrome.sessionID(from: selected) == "fixture")
for invalid: Any? in [nil, "broken JSON", "{}", #"{"sessionID":""}"#, Data([0xff])] {
    precondition(Chrome.sessionID(from: invalid) == nil)
}
let sidebar = Browser(session: "fixture", block: "web")
let page = sidebar.web
sidebar.shown = true
sidebar.focusWhenShown = true
sidebar.placement = placed
sidebar.shownSession = "other"
sidebar.follow()
sidebar.takeFocus()
precondition(!sidebar.shown && !sidebar.focusWhenShown && sidebar.placement?.focused == true)
sidebar.shownSession = "fixture"
precondition(sidebar.sessionIsShown && sidebar.web === page)
sidebar.shownSession = nil
precondition(sidebar.sessionIsShown) // unreadable preference: use session.view

sidebar.chrome = chrome
sidebar.lastFrame = CGRect(x: 100, y: 100, width: 400, height: 200) // AppKit origin
sidebar.lastExternalClick = (CGPoint(x: 300, y: 318), 10) // this pane's 36pt header
precondition(!sidebar.clickedOutsidePane(at: 10.25))
sidebar.lastExternalClick = (CGPoint(x: 300, y: 180), 10) // body
precondition(!sidebar.clickedOutsidePane(at: 10.25))
sidebar.lastExternalClick = (CGPoint(x: 50, y: 200), 10) // sidebar or sibling pane
precondition(sidebar.clickedOutsidePane(at: 10.25) && !sidebar.clickedOutsidePane(at: 10.76))

// Exercise activation reclaim during the settle interval without taking focus from the real Rex app.
if let front = NSWorkspace.shared.frontmostApplication {
    sidebar.rex = RexWindow(number: 0, pid: front.processIdentifier, bounds: .zero)
    sidebar.settleUntil = Date().addingTimeInterval(1)
    sidebar.lastExternalClick = (CGPoint(x: 50, y: 200), ProcessInfo.processInfo.systemUptime)
    sidebar.shown = true
    sidebar.reclaimFocusAfterActivation()
    precondition(!sidebar.window.isKeyWindow && !sidebar.focusRequestedByActivation && !sidebar.focusWhenShown)
    sidebar.shown = false // lastFrame may not yet include the new header: decide only once shown.
    sidebar.reclaimFocusAfterActivation()
    precondition(sidebar.focusRequestedByActivation && !sidebar.focusWhenShown)
    sidebar.shownSession = "other"
    sidebar.reclaimFocusAfterActivation()
    precondition(!sidebar.focusRequestedByActivation && !sidebar.focusWhenShown)
}
