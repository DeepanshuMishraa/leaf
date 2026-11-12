// leaf.dipxsy.app: the page for browsers, and the install script for
// `curl -fsSL https://leaf.dipxsy.app | sh`. Every other path is a static file.

/** Command-line clients that should get the script at the root, not the page. */
const SHELL_CLIENT = /^(curl|wget|fetch|httpie|xh|powershell)\b/i;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const agent = request.headers.get("user-agent") ?? "";
    const accept = request.headers.get("accept") ?? "";
    const wantsScript =
      url.pathname === "/install" ||
      (url.pathname === "/" && (SHELL_CLIENT.test(agent) || !accept.includes("text/html")));
    if (!wantsScript) {
      return env.ASSETS.fetch(request);
    }
    const script = await env.ASSETS.fetch(new Request(new URL("/install.sh", url), request));
    if (!script.ok) {
      return new Response("echo 'leaf install: the install script is missing from leaf.dipxsy.app' >&2; exit 1\n", {
        status: 500,
        headers: { "content-type": "text/plain; charset=utf-8" },
      });
    }
    return new Response(script.body, {
      headers: {
        "content-type": "text/plain; charset=utf-8",
        "cache-control": "public, max-age=300",
        vary: "user-agent, accept",
      },
    });
  },
};
