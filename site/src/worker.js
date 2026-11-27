// leaf.dipxsy.app: the page for browsers, and the install script for
// `curl -fsSL https://leaf.dipxsy.app | sh`, plus the sponsor API. Every other
// path is a static file.

import { sponsor } from "./sponsor.js";
import { list, webhook } from "./sponsors.js";

/** Command-line clients that should get the script at the root, not the page. */
const SHELL_CLIENT = /^(curl|wget|fetch|httpie|xh|powershell)\b/i;

/** Link unfurlers. A person still gets the Discord invite. */
const UNFURL =
  /discordbot|twitterbot|facebookexternalhit|facebot|slackbot|linkedinbot|telegrambot|whatsapp|embedly|pinterest|redditbot|applebot|bingpreview|skypeuripreview|quora link preview|vkshare|imessagebot/i;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname === "/api/sponsor") return sponsor(request, env);
    if (url.pathname === "/api/sponsors") return list(env);
    if (url.pathname === "/api/dodo-webhook") return webhook(request, env);
    if (url.pathname === "/discord" || url.pathname === "/discord/") {
      const previewAgent = request.headers.get("user-agent") ?? "";
      if (UNFURL.test(previewAgent)) {
        // The page's canonical path: Cloudflare's HTML handling answers
        // `/discord.html` with a redirect to `/discord`, not the page.
        const page = await env.ASSETS.fetch(new Request(new URL("/discord", url)));
        return new Response(page.body, {
          status: page.status,
          headers: {
            "content-type": "text/html; charset=utf-8",
            "cache-control": "public, max-age=300",
            // People get a redirect from the same path: a cache must keep them apart.
            vary: "user-agent",
          },
        });
      }
      return Response.redirect("https://discord.gg/3XFF5gFEu", 302);
    }
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
