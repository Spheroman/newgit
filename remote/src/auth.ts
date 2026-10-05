// Who is calling. Two credentials, never confused:
//
// - A newgit token says who you are, to this Worker. It is signed, not
//   stored: `ngt_<payload>.<signature>`, HMAC-SHA256 over the payload with
//   TOKEN_KEY. Rotating TOKEN_KEY revokes every token at once.
// - An Artifacts repo token says what one repo lets you do, for minutes. The
//   Worker mints those per request (see index.ts) and never stores them.

const PREFIX = "ngt_";
const TOKEN_TTL_SECONDS = 60 * 60 * 24 * 30;

interface Payload {
	/** GitHub login, lowercased. */
	sub: string;
	iat: number;
	exp: number;
}

const encoder = new TextEncoder();

function base64url(bytes: Uint8Array): string {
	let binary = "";
	for (const byte of bytes) binary += String.fromCharCode(byte);
	return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

function unbase64url(text: string): Uint8Array {
	const padded = text.replaceAll("-", "+").replaceAll("_", "/") + "===".slice((text.length + 3) % 4);
	return Uint8Array.from(atob(padded), (char) => char.charCodeAt(0));
}

async function hmacKey(secret: string): Promise<CryptoKey> {
	return crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
		"sign",
		"verify",
	]);
}

export async function issueToken(secret: string, login: string, now = Date.now()): Promise<string> {
	const iat = Math.floor(now / 1000);
	const payload: Payload = { sub: login.toLowerCase(), iat, exp: iat + TOKEN_TTL_SECONDS };
	const body = base64url(encoder.encode(JSON.stringify(payload)));
	const signature = await crypto.subtle.sign("HMAC", await hmacKey(secret), encoder.encode(body));
	return `${PREFIX}${body}.${base64url(new Uint8Array(signature))}`;
}

/** The login a valid, unexpired token was issued to; null for anything else. */
export async function verifyToken(secret: string, token: string, now = Date.now()): Promise<string | null> {
	if (!token.startsWith(PREFIX)) return null;
	const [body, signature, extra] = token.slice(PREFIX.length).split(".");
	if (!body || !signature || extra !== undefined) return null;
	let valid: boolean;
	try {
		valid = await crypto.subtle.verify("HMAC", await hmacKey(secret), unbase64url(signature), encoder.encode(body));
	} catch {
		return null;
	}
	if (!valid) return null;
	let payload: Payload;
	try {
		payload = JSON.parse(new TextDecoder().decode(unbase64url(body)));
	} catch {
		return null;
	}
	if (typeof payload.sub !== "string" || typeof payload.exp !== "number") return null;
	if (payload.exp * 1000 <= now) return null;
	return payload.sub;
}

// GitHub OAuth device flow — what `gh auth login` does. The CLI shows a code,
// the user approves it in a browser, the CLI polls. No callback server.

export interface DeviceCode {
	device_code: string;
	user_code: string;
	verification_uri: string;
	interval: number;
	expires_in: number;
}

export async function startDeviceFlow(clientId: string, fetcher: typeof fetch = fetch): Promise<DeviceCode> {
	const response = await fetcher("https://github.com/login/device/code", {
		method: "POST",
		headers: { accept: "application/json", "content-type": "application/json" },
		body: JSON.stringify({ client_id: clientId, scope: "" }),
	});
	if (!response.ok) throw new Error(`GitHub device flow: ${response.status}`);
	return response.json();
}

export type PollResult =
	| { status: "pending"; interval?: number }
	| { status: "done"; login: string }
	| { status: "failed"; error: string };

/** One poll. Pending covers both `authorization_pending` and `slow_down`. */
export async function pollDeviceFlow(
	clientId: string,
	deviceCode: string,
	fetcher: typeof fetch = fetch,
): Promise<PollResult> {
	const response = await fetcher("https://github.com/login/oauth/access_token", {
		method: "POST",
		headers: { accept: "application/json", "content-type": "application/json" },
		body: JSON.stringify({
			client_id: clientId,
			device_code: deviceCode,
			grant_type: "urn:ietf:params:oauth:grant-type:device_code",
		}),
	});
	const body = (await response.json()) as { access_token?: string; error?: string; interval?: number };
	if (body.error === "authorization_pending") return { status: "pending" };
	if (body.error === "slow_down") return { status: "pending", interval: body.interval };
	if (!body.access_token) return { status: "failed", error: body.error ?? `GitHub answered ${response.status}` };

	// The GitHub token is used once, to learn the login, and then dropped:
	// newgit never acts on GitHub on the user's behalf.
	const user = await fetcher("https://api.github.com/user", {
		headers: { authorization: `Bearer ${body.access_token}`, "user-agent": "newgit", accept: "application/json" },
	});
	if (!user.ok) return { status: "failed", error: `GitHub /user answered ${user.status}` };
	const { login } = (await user.json()) as { login: string };
	return { status: "done", login: login.toLowerCase() };
}
