import AppKit

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
