package io.ventic.venticode;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.apache.camel.Endpoint;
import org.apache.camel.Exchange;
import org.apache.camel.Producer;
import org.apache.camel.builder.RouteBuilder;
import org.apache.camel.component.http.HttpComponent;
import org.apache.camel.component.jetty.JettyHttpComponent;
import org.apache.camel.http.base.HttpHeaderFilterStrategy;
import org.apache.camel.main.Main;
import org.apache.camel.spi.Synchronization;
import org.apache.camel.support.UnitOfWorkHelper;
import org.apache.hc.client5.http.config.ConnectionConfig;
import org.apache.hc.client5.http.impl.io.PoolingHttpClientConnectionManager;
import org.apache.hc.client5.http.impl.io.PoolingHttpClientConnectionManagerBuilder;
import org.apache.hc.core5.util.TimeValue;
import org.apache.hc.core5.util.Timeout;

import javax.net.ssl.SSLHandshakeException;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLProtocolException;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.URI;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.time.Duration;
import java.time.Instant;
import java.time.ZonedDateTime;
import java.time.format.DateTimeFormatter;
import java.time.format.DateTimeParseException;
import java.util.HexFormat;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;

/** Fixed provider operations reachable only on the Venticode pod's loopback interface. */
public final class Connector extends RouteBuilder {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String GITLAB = System.getenv().getOrDefault("GITLAB_ORIGIN", "https://gitlab.com");
    private static final String PORT = System.getenv().getOrDefault("CONNECTOR_PORT", "4190");

    // Timeout budget strictly below backend's 20s HTTP timeout (2s + 3s + 12s = 17s).
    private static final int CONNECTION_REQUEST_TIMEOUT_MS = 2_000;
    private static final int CONNECT_TIMEOUT_MS = 3_000;
    private static final int RESPONSE_TIMEOUT_MS = 12_000;
    private static final long TOTAL_BUDGET_MS = 17_000;
    private static final long RETRY_BUDGET_NANOS = TimeUnit.MILLISECONDS.toNanos(5_000);
    private static final int SHUTDOWN_TIMEOUT_SECONDS = 15;

    // Hard memory bounds for inbound and upstream HTTP bodies (2 MiB).
    private static final int MAX_BODY_BYTES = 2 * 1024 * 1024;
    private static final int MAX_FIELD_CHARS = 65_536;
    private static final int MAX_TELEGRAM_TEXT_CHARS = 4_096;
    private static final int MAX_GITLAB_TEXT_CHARS = 1_000_000;
    private static final int MAX_IDEMPOTENCY_KEY_CHARS = 256;

    // Connection pool and Jetty thread pool bounds.
    private static final int POOL_MAX_TOTAL = 128;
    private static final int POOL_MAX_PER_ROUTE = 32;
    private static final int POOL_CONN_TTL_SECONDS = 60;
    private static final int POOL_VALIDATE_INACTIVITY_SECONDS = 2;
    private static final int POOL_EVICT_IDLE_SECONDS = 30;
    private static final int JETTY_MIN_THREADS = 8;
    private static final int JETTY_MAX_THREADS = 64;
    private static final int JETTY_MAX_REQUEST_HEADER_BYTES = 16 * 1024;

    // Retry-After bounds and idempotent retry policy.
    private static final int MIN_RETRY_AFTER_SECONDS = 1;
    private static final int MAX_RETRY_AFTER_SECONDS = 300;
    private static final int MAX_IDEMPOTENT_RETRIES = 2;
    private static final long[] RETRY_BACKOFF_MS = {150L, 350L};

    private static final Set<String> KNOWN_KINDS = Set.of("slack", "telegram", "gitlab");
    private static final Set<String> KNOWN_OPERATIONS = Set.of(
        "exchange", "identify", "inspect", "hooks", "install", "remove", "reply");
    private static final Set<String> IDEMPOTENT_OPERATIONS = Set.of(
        "identify", "inspect", "hooks", "remove");

    private static final byte[] ERR_INVALID = "{\"error\":\"Invalid connector request\"}".getBytes(StandardCharsets.UTF_8);
    private static final byte[] ERR_TOO_LARGE = "{\"error\":\"Request body too large\"}".getBytes(StandardCharsets.UTF_8);
    private static final byte[] ERR_FAILED = "{\"error\":\"Connector request failed\"}".getBytes(StandardCharsets.UTF_8);
    private static final byte[] ERR_REJECTED = "{\"error\":\"Provider rejected operation\"}".getBytes(StandardCharsets.UTF_8);

    private final ReplyIdempotencyCache replyCache = new ReplyIdempotencyCache();
    private Endpoint upstreamEndpoint;
    private Producer upstreamProducer;

    public static void main(String[] args) throws Exception {
        URI gitlab = URI.create(GITLAB);
        if (!"https".equals(gitlab.getScheme()) || gitlab.getHost() == null || gitlab.getUserInfo() != null
                || !gitlab.getRawPath().isEmpty() || gitlab.getQuery() != null || gitlab.getFragment() != null)
            throw new IllegalArgumentException("GITLAB_ORIGIN must be a fixed HTTPS origin");
        int port = Integer.parseInt(PORT);
        if (port < 1 || port > 65535)
            throw new IllegalArgumentException("CONNECTOR_PORT must be between 1 and 65535");
        Main main = new Main();
        main.configure().setShutdownTimeout(SHUTDOWN_TIMEOUT_SECONDS);
        main.configure().setShutdownLogInflightExchangesOnTimeout(false);
        main.configure().addRoutesBuilder(new Connector());
        main.run();
    }

    @Override
    public void configure() throws Exception {
        // Provider responses can contain tokens. Never log exchange bodies, exceptions or request URIs.
        getContext().getShutdownStrategy().setTimeout(SHUTDOWN_TIMEOUT_SECONDS);
        getContext().getShutdownStrategy().setLogInflightExchangesOnTimeout(false);

        var jetty = getContext().getComponent("jetty", JettyHttpComponent.class);
        jetty.setMinThreads(JETTY_MIN_THREADS);
        jetty.setMaxThreads(JETTY_MAX_THREADS);
        jetty.setRequestHeaderSize(JETTY_MAX_REQUEST_HEADER_BYTES);
        jetty.setSendServerVersion(false);

        ConnectionConfig connConfig = ConnectionConfig.custom()
            .setConnectTimeout(Timeout.ofMilliseconds(CONNECT_TIMEOUT_MS))
            .setSocketTimeout(Timeout.ofMilliseconds(RESPONSE_TIMEOUT_MS))
            .setTimeToLive(TimeValue.ofSeconds(POOL_CONN_TTL_SECONDS))
            .setValidateAfterInactivity(TimeValue.ofSeconds(POOL_VALIDATE_INACTIVITY_SECONDS))
            .build();
        PoolingHttpClientConnectionManager pool = PoolingHttpClientConnectionManagerBuilder.create()
            .useSystemProperties()
            .setMaxConnTotal(POOL_MAX_TOTAL)
            .setMaxConnPerRoute(POOL_MAX_PER_ROUTE)
            .setDefaultConnectionConfig(connConfig)
            .build();

        var https = getContext().getComponent("https", HttpComponent.class);
        https.setClientConnectionManager(pool);
        https.setHttpClientConfigurer(builder -> builder
            .evictExpiredConnections()
            .evictIdleConnections(TimeValue.ofSeconds(POOL_EVICT_IDLE_SECONDS)));
        https.setHeaderFilterStrategy(new RetryAfterOnlyHeaderFilterStrategy());
        https.setAutomaticRetriesDisabled(true);
        https.setRedirectHandlingDisabled(true);
        https.setCookieManagementDisabled(true);
        https.setAuthCachingDisabled(true);
        https.setConnectionStateDisabled(true);
        https.setConnectionRequestTimeout(CONNECTION_REQUEST_TIMEOUT_MS);
        https.setConnectTimeout(CONNECT_TIMEOUT_MS);
        https.setResponseTimeout(RESPONSE_TIMEOUT_MS);
        https.setSoTimeout(RESPONSE_TIMEOUT_MS);

        this.upstreamEndpoint = getContext().getEndpoint(
            "https://localhost?throwExceptionOnFailure=false&followRedirects=false&disableStreamCache=true"
                + "&connectTimeout=" + CONNECT_TIMEOUT_MS
                + "&connectionRequestTimeout=" + CONNECTION_REQUEST_TIMEOUT_MS
                + "&responseTimeout=" + RESPONSE_TIMEOUT_MS);
        this.upstreamProducer = upstreamEndpoint.createProducer();
        getContext().addService(upstreamProducer);

        errorHandler(defaultErrorHandler()
            .maximumRedeliveries(0)
            .logExhausted(false)
            .logExhaustedMessageHistory(false)
            .logExhaustedMessageBody(false)
            .logStackTrace(false));

        from("jetty:http://127.0.0.1:" + PORT + "/healthz?httpMethodRestrict=GET")
            .routeId("healthz")
            .setHeader(Exchange.CONTENT_TYPE, constant("text/plain"))
            .setHeader(Exchange.HTTP_RESPONSE_CODE, constant(200))
            .setBody(constant("ok"));

        from("jetty:http://127.0.0.1:" + PORT + "/exchange?httpMethodRestrict=POST&disableStreamCache=true")
            .routeId("provider-exchange")
            .streamCache(false)
            .onException(PayloadTooLargeException.class).handled(true)
                .process(e -> {
                    writeResponse(e, 413, ERR_TOO_LARGE, null);
                    logExchange(prop(e, "kind"), prop(e, "operation"), 413, 0, e.getProperty(Exchange.EXCEPTION_CAUGHT, Throwable.class));
                })
            .end()
            .onException(IllegalArgumentException.class).handled(true)
                .process(e -> {
                    writeResponse(e, 400, ERR_INVALID, null);
                    logExchange(prop(e, "kind"), prop(e, "operation"), 400, 0, e.getProperty(Exchange.EXCEPTION_CAUGHT, Throwable.class));
                })
            .end()
            .onException(Exception.class).handled(true)
                .process(e -> {
                    writeResponse(e, 502, ERR_FAILED, null);
                    logExchange(prop(e, "kind"), prop(e, "operation"), 502, 0, e.getProperty(Exchange.EXCEPTION_CAUGHT, Throwable.class));
                })
            .end()
            .process(this::handleExchange);
    }

    private void handleExchange(Exchange e) {
        long startedNanos = System.nanoTime();
        try {
            PreparedRequest req = prepare(e);
            ProcessedResponse out = req.cacheKey() != null
                ? executeIdempotentReply(req, startedNanos)
                : executeUpstream(req, startedNanos);
            writeResponse(e, out.status(), out.body(), out.retryAfterSeconds());
            logExchange(req.kind(), req.operation(), out.status(), elapsedMs(startedNanos), out.error());
        } catch (PayloadTooLargeException ex) {
            writeResponse(e, 413, ERR_TOO_LARGE, null);
            logExchange(prop(e, "kind"), prop(e, "operation"), 413, elapsedMs(startedNanos), ex);
        } catch (IllegalArgumentException ex) {
            writeResponse(e, 400, ERR_INVALID, null);
            logExchange(prop(e, "kind"), prop(e, "operation"), 400, elapsedMs(startedNanos), ex);
        } catch (Exception ex) {
            writeResponse(e, 502, ERR_FAILED, null);
            logExchange(prop(e, "kind"), prop(e, "operation"), 502, elapsedMs(startedNanos), ex);
        }
    }

    private ProcessedResponse executeIdempotentReply(PreparedRequest req, long startedNanos) throws Exception {
        String key = req.cacheKey();
        CachedReply cached = replyCache.get(key);
        if (cached != null) {
            return new ProcessedResponse(cached.status(), cached.body(), null, null);
        }

        CompletableFuture<ProcessedResponse> mine = new CompletableFuture<>();
        CompletableFuture<ProcessedResponse> existing = replyCache.inFlight.putIfAbsent(key, mine);
        if (existing != null) {
            long remainingMs = Math.max(100L, TOTAL_BUDGET_MS - elapsedMs(startedNanos));
            try {
                return existing.get(remainingMs, TimeUnit.MILLISECONDS);
            } catch (InterruptedException ie) {
                Thread.currentThread().interrupt();
                throw new IOException("Interrupted waiting for in-flight reply");
            } catch (TimeoutException te) {
                throw new IOException("Timed out waiting for in-flight reply");
            } catch (ExecutionException ee) {
                if (ee.getCause() instanceof Exception cause) throw cause;
                throw new IOException("In-flight reply failed");
            }
        }

        try {
            // Re-check cache after winning in-flight registration to close race window.
            CachedReply afterLock = replyCache.get(key);
            if (afterLock != null) {
                ProcessedResponse hit = new ProcessedResponse(afterLock.status(), afterLock.body(), null, null);
                mine.complete(hit);
                return hit;
            }
            ProcessedResponse res = executeUpstream(req, startedNanos);
            if (res.status() == 200) {
                replyCache.put(key, res.status(), res.body());
            }
            mine.complete(res);
            return res;
        } catch (Exception ex) {
            mine.completeExceptionally(ex);
            throw ex;
        } finally {
            replyCache.inFlight.remove(key, mine);
            if (!mine.isDone()) {
                mine.completeExceptionally(new IOException("In-flight reply aborted"));
            }
        }
    }

    private ProcessedResponse executeUpstream(PreparedRequest req, long startedNanos) throws Exception {
        boolean idempotent = IDEMPOTENT_OPERATIONS.contains(req.operation());
        int maxAttempts = idempotent ? (1 + MAX_IDEMPOTENT_RETRIES) : 1;

        for (int attempt = 1; attempt <= maxAttempts; attempt++) {
            Exchange child = upstreamEndpoint.createExchange();
            UpstreamResponse upstream;
            try {
                child.getMessage().setHeader(Exchange.HTTP_URI, req.uri());
                if (req.rawQuery() != null) {
                    child.getMessage().setHeader(Exchange.HTTP_QUERY, req.rawQuery());
                }
                child.getMessage().setHeader(Exchange.HTTP_METHOD, req.method());
                if (req.authHeaderName() != null) {
                    child.getMessage().setHeader(req.authHeaderName(), req.authHeaderValue());
                }
                if (req.body() != null) {
                    child.getMessage().setHeader(Exchange.CONTENT_TYPE, req.contentType());
                    child.getMessage().setBody(req.body());
                } else {
                    child.getMessage().setBody(null);
                }

                upstreamProducer.process(child);
                Exception err = child.getException();
                if (err != null) {
                    throw err;
                }
                upstream = readUpstreamResponse(child);
            } catch (Exception err) {
                if (attempt < maxAttempts && isRetryableException(err) && sleepForRetry(attempt, startedNanos)) {
                    continue;
                }
                throw err;
            } finally {
                List<Synchronization> completions = child.getExchangeExtension().handoverCompletions();
                if (completions != null && !completions.isEmpty()) {
                    UnitOfWorkHelper.doneSynchronizations(child, completions);
                }
            }

            if (attempt < maxAttempts && isRetryableStatus(upstream.status()) && sleepForRetry(attempt, startedNanos)) {
                continue;
            }
            return processUpstreamResponse(req.operation(), upstream);
        }
        throw new IOException("Upstream retry attempts exhausted");
    }

    private UpstreamResponse readUpstreamResponse(Exchange e) throws IOException {
        int status = e.getMessage().getHeader(Exchange.HTTP_RESPONSE_CODE, 502, Integer.class);
        Object raHeader = e.getMessage().getHeader("Retry-After");
        String retryAfterRaw = null;
        if (raHeader instanceof List<?> list && !list.isEmpty() && list.get(0) != null) {
            retryAfterRaw = list.get(0).toString();
        } else if (raHeader != null) {
            retryAfterRaw = raHeader.toString();
        }

        Object rawBody = e.getMessage().getBody();
        BoundedRead read;
        if (rawBody == null) {
            read = new BoundedRead(new byte[0], false);
        } else if (rawBody instanceof byte[] bytes) {
            read = bytes.length > MAX_BODY_BYTES
                ? new BoundedRead(new byte[0], true)
                : new BoundedRead(bytes, false);
        } else if (rawBody instanceof String str) {
            if (str.length() > MAX_BODY_BYTES) {
                read = new BoundedRead(new byte[0], true);
            } else {
                byte[] bytes = str.getBytes(StandardCharsets.UTF_8);
                read = bytes.length > MAX_BODY_BYTES
                    ? new BoundedRead(new byte[0], true)
                    : new BoundedRead(bytes, false);
            }
        } else {
            InputStream in = rawBody instanceof InputStream is ? is : e.getMessage().getBody(InputStream.class);
            if (in == null) {
                read = new BoundedRead(new byte[0], false);
            } else {
                try (in) {
                    read = readStreamBounded(in, MAX_BODY_BYTES);
                }
            }
        }

        e.getMessage().removeHeaders("*");
        return new UpstreamResponse(status, retryAfterRaw, read.bytes(), read.exceeded());
    }

    private ProcessedResponse processUpstreamResponse(String op, UpstreamResponse upstream) throws IOException {
        int status = upstream.status();
        if ("remove".equals(op) && status == 404) {
            return new ProcessedResponse(200, JSON.writeValueAsBytes(JSON.createObjectNode().put("ok", true)), null, null);
        }

        JsonNode result = null;
        Throwable parseError = null;
        if (upstream.truncated()) {
            parseError = new UpstreamTooLargeException();
        } else if (upstream.body().length == 0 || isBlankUtf8(upstream.body())) {
            result = JSON.createObjectNode();
        } else {
            try {
                result = JSON.readTree(upstream.body());
            } catch (JsonProcessingException ex) {
                parseError = ex;
            }
        }

        if ("remove".equals(op) && result != null && result.isObject()) {
            String error = result.path("error").asText("");
            if ("token_revoked".equals(error) || "invalid_auth".equals(error)) {
                return new ProcessedResponse(200, JSON.writeValueAsBytes(JSON.createObjectNode().put("ok", true)), null, null);
            }
        }

        // Preserve upstream 401/403/429 even when body is non-JSON (HTML/plain text) or oversized.
        if (status == 401 || status == 403 || status == 429) {
            Integer retryAfter = null;
            byte[] errBody = ERR_REJECTED;
            if (status == 429) {
                retryAfter = parseRetryAfterHeader(upstream.retryAfterHeader());
                if (retryAfter == null) {
                    retryAfter = parseTelegramRetryAfter(result);
                }
                if (retryAfter != null) {
                    ObjectNode node = JSON.createObjectNode()
                        .put("error", "Provider rejected operation")
                        .put("retry_after", retryAfter);
                    errBody = JSON.writeValueAsBytes(node);
                }
            }
            return new ProcessedResponse(status, errBody, retryAfter, parseError);
        }

        if (status < 200 || status >= 300 || parseError != null || result == null || !result.path("ok").asBoolean(true)) {
            return new ProcessedResponse(502, ERR_REJECTED, null, parseError);
        }

        return new ProcessedResponse(200, JSON.writeValueAsBytes(result), null, null);
    }

    private PreparedRequest prepare(Exchange e) throws IOException {
        byte[] raw = readInboundBody(e);
        if (raw.length == 0) {
            throw new IllegalArgumentException("Empty connector request");
        }
        final JsonNode req;
        try {
            req = JSON.readTree(raw);
        } catch (JsonProcessingException ex) {
            throw new IllegalArgumentException("Malformed connector JSON");
        }
        if (req == null || !req.isObject()) {
            throw new IllegalArgumentException("Connector request must be a JSON object");
        }

        String kind = field(req, "kind");
        e.setProperty("kind", kind);
        String op = field(req, "operation");
        e.setProperty("operation", op);

        JsonNode c = req.path("credentials");
        if (!c.isObject()) {
            throw new IllegalArgumentException("Missing credentials");
        }

        String idempotencyKey = null;
        if ("reply".equals(op)) {
            JsonNode ik = req.path("idempotency_key");
            if (!ik.isMissingNode() && !ik.isNull()) {
                if (!ik.isTextual()) {
                    throw new IllegalArgumentException("Invalid idempotency_key");
                }
                String value = ik.asText("");
                if (value.isEmpty() || value.length() > MAX_IDEMPOTENCY_KEY_CHARS) {
                    throw new IllegalArgumentException("Invalid idempotency_key");
                }
                idempotencyKey = value;
            }
        }

        String method = "POST";
        String url;
        String authHeaderName = null;
        String authHeaderValue = null;
        String destinationForKey = null;
        ObjectNode body = JSON.createObjectNode();
        String form = null;

        switch (kind) {
            case "slack" -> {
                url = "https://slack.com/api/";
                switch (op) {
                    case "exchange" -> {
                        url += "oauth.v2.access";
                        form = "client_id=" + encoded(field(c, "client_id")) + "&client_secret=" + encoded(field(c, "client_secret"))
                            + "&code=" + encoded(field(req, "code")) + "&redirect_uri=" + encoded(field(req, "redirect_uri"));
                    }
                    case "identify" -> url += "auth.test";
                    case "remove" -> url += "auth.revoke";
                    case "inspect" -> {
                        url += "conversations.info?channel=" + encoded(field(req, "destination"));
                        method = "GET";
                    }
                    case "reply" -> {
                        url += "chat.postMessage";
                        destinationForKey = field(req, "destination");
                        body.put("channel", destinationForKey)
                            .put("thread_ts", field(req, "thread"))
                            .put("text", field(req, "text"));
                    }
                    default -> throw new IllegalArgumentException("Unknown Slack operation");
                }
                if (!op.equals("exchange")) {
                    authHeaderName = "Authorization";
                    authHeaderValue = "Bearer " + field(c, "token");
                }
            }
            case "telegram" -> {
                String token = field(c, "token");
                if (!token.matches("[0-9]+:[A-Za-z0-9_-]+")) throw new IllegalArgumentException("Invalid Telegram identity");
                url = "https://api.telegram.org/bot" + token + "/";
                switch (op) {
                    case "identify" -> url += "getMe";
                    case "inspect" -> {
                        url += "getChat";
                        body.put("chat_id", field(req, "destination"));
                    }
                    case "install" -> {
                        url += "setWebhook";
                        body.put("url", httpsUrl(req, "webhook_url"))
                            .put("secret_token", field(c, "webhook_secret"));
                        body.putArray("allowed_updates").add("message");
                    }
                    case "remove" -> url += "deleteWebhook";
                    case "reply" -> {
                        url += "sendMessage";
                        destinationForKey = field(req, "destination");
                        body.put("chat_id", destinationForKey)
                            .put("text", field(req, "text", MAX_TELEGRAM_TEXT_CHARS))
                            .putObject("reply_parameters")
                            .put("message_id", numericLong(req, "thread"));
                    }
                    default -> throw new IllegalArgumentException("Unknown Telegram operation");
                }
            }
            case "gitlab" -> {
                authHeaderName = "PRIVATE-TOKEN";
                authHeaderValue = field(c, "token");
                if ("identify".equals(op)) {
                    url = GITLAB + "/api/v4/user";
                    method = "GET";
                } else {
                    String projectId = numeric(req, "destination");
                    url = GITLAB + "/api/v4/projects/" + projectId;
                    switch (op) {
                        case "inspect" -> method = "GET";
                        case "hooks" -> {
                            url += "/hooks?per_page=100&page=" + numeric(req, "page");
                            method = "GET";
                        }
                        case "install" -> {
                            url += "/hooks";
                            JsonNode hookId = req.path("hook_id");
                            if (!hookId.isMissingNode() && !hookId.isNull()) {
                                url += "/" + numeric(req, "hook_id");
                                method = "PUT";
                            }
                            body.put("url", httpsUrl(req, "webhook_url"))
                                .put("token", field(c, "webhook_secret"))
                                .put("note_events", true)
                                .put("push_events", false)
                                .put("enable_ssl_verification", true);
                        }
                        case "remove" -> {
                            url += "/hooks/" + numeric(req, "hook_id");
                            method = "DELETE";
                        }
                        case "reply" -> {
                            destinationForKey = projectId;
                            String type = field(req, "object_type");
                            if (!type.equals("issues") && !type.equals("merge_requests")) {
                                throw new IllegalArgumentException("Unknown GitLab object");
                            }
                            url += "/" + type + "/" + numeric(req, "iid");
                            String thread = field(req, "thread");
                            if (!thread.equals("root") && !thread.matches("[a-zA-Z0-9_-]{1,128}")) {
                                throw new IllegalArgumentException("Invalid GitLab discussion");
                            }
                            url += thread.equals("root") ? "/notes" : "/discussions/" + thread + "/notes";
                            body.put("body", field(req, "text", MAX_GITLAB_TEXT_CHARS));
                        }
                        default -> throw new IllegalArgumentException("Unknown GitLab operation");
                    }
                }
            }
            default -> throw new IllegalArgumentException("Unknown provider");
        }

        URI target = URI.create(url);
        String baseUri = url.split("\\?", 2)[0];
        boolean hasBody = method.equals("POST") || method.equals("PUT");
        String contentType = null;
        byte[] payload = null;
        if (hasBody) {
            contentType = form == null ? "application/json" : "application/x-www-form-urlencoded";
            payload = form == null ? JSON.writeValueAsBytes(body) : form.getBytes(StandardCharsets.UTF_8);
        }
        String cacheKey = idempotencyKey != null
            ? sha256Hex(kind + "\0" + (destinationForKey == null ? "" : destinationForKey) + "\0" + idempotencyKey)
            : null;
        return new PreparedRequest(kind, op, method, baseUri, target.getRawQuery(), authHeaderName, authHeaderValue, contentType, payload, cacheKey);
    }

    private static byte[] readInboundBody(Exchange e) throws IOException {
        Object cl = e.getMessage().getHeader(Exchange.CONTENT_LENGTH);
        if (cl == null) cl = e.getMessage().getHeader("Content-Length");
        if (cl != null) {
            try {
                long len = Long.parseLong(cl.toString().trim());
                if (len > MAX_BODY_BYTES) throw new PayloadTooLargeException();
                if (len < 0) throw new IllegalArgumentException("Invalid Content-Length");
            } catch (NumberFormatException ignored) {
                // Fall through to bounded stream read.
            }
        }
        Object body = e.getMessage().getBody();
        if (body == null) return new byte[0];
        if (body instanceof byte[] bytes) {
            if (bytes.length > MAX_BODY_BYTES) throw new PayloadTooLargeException();
            return bytes;
        }
        if (body instanceof String str) {
            if (str.length() > MAX_BODY_BYTES) throw new PayloadTooLargeException();
            byte[] bytes = str.getBytes(StandardCharsets.UTF_8);
            if (bytes.length > MAX_BODY_BYTES) throw new PayloadTooLargeException();
            return bytes;
        }
        InputStream in = body instanceof InputStream is ? is : e.getMessage().getBody(InputStream.class);
        if (in == null) return new byte[0];
        try (in) {
            BoundedRead read = readStreamBounded(in, MAX_BODY_BYTES);
            if (read.exceeded()) throw new PayloadTooLargeException();
            return read.bytes();
        }
    }

    private static BoundedRead readStreamBounded(InputStream in, int maxBytes) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream(Math.min(4096, maxBytes));
        byte[] buf = new byte[8192];
        int total = 0;
        int n;
        while ((n = in.read(buf)) != -1) {
            if (total + n > maxBytes) {
                return new BoundedRead(new byte[0], true);
            }
            out.write(buf, 0, n);
            total += n;
        }
        return new BoundedRead(out.toByteArray(), false);
    }

    private static boolean isBlankUtf8(byte[] bytes) {
        for (byte b : bytes) {
            if (b != ' ' && b != '\t' && b != '\r' && b != '\n') return false;
        }
        return true;
    }

    private static boolean isRetryableStatus(int status) {
        return status == 502 || status == 503 || status == 504;
    }

    private static boolean isRetryableException(Throwable ex) {
        boolean sawIo = false;
        for (Throwable cur = ex; cur != null; cur = cur.getCause()) {
            if (cur instanceof SSLHandshakeException
                    || cur instanceof SSLPeerUnverifiedException
                    || cur instanceof SSLProtocolException
                    || cur instanceof InterruptedException) {
                return false;
            }
            if (cur instanceof IOException) {
                sawIo = true;
            }
        }
        return sawIo;
    }

    private static boolean sleepForRetry(int attempt, long startedNanos) {
        long backoffMs = RETRY_BACKOFF_MS[Math.min(attempt - 1, RETRY_BACKOFF_MS.length - 1)];
        long elapsedNanos = System.nanoTime() - startedNanos;
        if (elapsedNanos + TimeUnit.MILLISECONDS.toNanos(backoffMs) >= RETRY_BUDGET_NANOS) {
            return false;
        }
        try {
            Thread.sleep(backoffMs);
            return true;
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
            return false;
        }
    }

    private static Integer parseRetryAfterHeader(String raw) {
        if (raw == null) return null;
        String s = raw.trim();
        if (s.isEmpty() || s.length() > 64) return null;
        if (s.matches("[0-9]+")) {
            if (s.length() > 9) return MAX_RETRY_AFTER_SECONDS;
            long val = Long.parseLong(s);
            return (int) Math.max(MIN_RETRY_AFTER_SECONDS, Math.min(MAX_RETRY_AFTER_SECONDS, val));
        }
        try {
            Instant target = ZonedDateTime.parse(s, DateTimeFormatter.RFC_1123_DATE_TIME).toInstant();
            long delta = Duration.between(Instant.now(), target).getSeconds();
            return (int) Math.max(MIN_RETRY_AFTER_SECONDS, Math.min(MAX_RETRY_AFTER_SECONDS, delta));
        } catch (DateTimeParseException ignored) {
            return null;
        }
    }

    private static Integer parseTelegramRetryAfter(JsonNode result) {
        if (result == null || !result.isObject()) return null;
        JsonNode node = result.path("parameters").path("retry_after");
        if (node.isNumber()) {
            long val = node.asLong();
            return (int) Math.max(MIN_RETRY_AFTER_SECONDS, Math.min(MAX_RETRY_AFTER_SECONDS, val));
        }
        if (node.isTextual()) {
            return parseRetryAfterHeader(node.asText());
        }
        return null;
    }

    private static void writeResponse(Exchange e, int status, byte[] body, Integer retryAfterSeconds) {
        e.getMessage().removeHeaders("*");
        e.getMessage().setHeader(Exchange.HTTP_RESPONSE_CODE, status);
        e.getMessage().setHeader(Exchange.CONTENT_TYPE, "application/json");
        if (status == 429 && retryAfterSeconds != null) {
            e.getMessage().setHeader("Retry-After", Integer.toString(retryAfterSeconds));
        }
        e.getMessage().setBody(body);
    }

    private static void logExchange(String kind, String operation, int status, long latencyMs, Throwable error) {
        try {
            ObjectNode line = JSON.createObjectNode();
            line.put("kind", kind != null && KNOWN_KINDS.contains(kind) ? kind : "unknown");
            line.put("operation", operation != null && KNOWN_OPERATIONS.contains(operation) ? operation : "unknown");
            line.put("status", status);
            line.put("latencyMs", Math.max(0L, latencyMs));
            if (error != null) {
                line.put("exception", error.getClass().getSimpleName());
            } else {
                line.putNull("exception");
            }
            System.err.println(JSON.writeValueAsString(line));
        } catch (Exception ignored) {
            // Never fail a request due to logging.
        }
    }

    private static String prop(Exchange e, String key) {
        Object val = e.getProperty(key);
        return val instanceof String s ? s : null;
    }

    private static long elapsedMs(long startedNanos) {
        return TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - startedNanos);
    }

    private static String field(JsonNode n, String key) {
        return field(n, key, MAX_FIELD_CHARS);
    }

    private static String field(JsonNode n, String key, int maxLen) {
        JsonNode node = n.path(key);
        if (node.isMissingNode() || node.isNull() || node.isContainerNode() || node.isBoolean()) {
            throw new IllegalArgumentException("Missing connector field");
        }
        String value = node.asText("");
        if (value.isEmpty() || value.length() > maxLen) {
            throw new IllegalArgumentException("Missing connector field");
        }
        return value;
    }

    private static String httpsUrl(JsonNode n, String key) {
        String value = field(n, key);
        URI uri = URI.create(value);
        if (!"https".equals(uri.getScheme()) || uri.getHost() == null || uri.getUserInfo() != null)
            throw new IllegalArgumentException("HTTPS URL required");
        return value;
    }

    private static long numericLong(JsonNode n, String key) {
        String value = field(n, key);
        if (!value.matches("[0-9]{1,19}")) throw new IllegalArgumentException("Invalid number");
        try {
            return Long.parseLong(value);
        } catch (NumberFormatException ex) {
            throw new IllegalArgumentException("Number out of range");
        }
    }

    private static String numeric(JsonNode n, String key) {
        return Long.toString(numericLong(n, key));
    }

    private static String encoded(String s) {
        return URLEncoder.encode(s, StandardCharsets.UTF_8);
    }

    private static String sha256Hex(String input) {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            return HexFormat.of().formatHex(md.digest(input.getBytes(StandardCharsets.UTF_8)));
        } catch (NoSuchAlgorithmException ex) {
            throw new IllegalStateException("SHA-256 unavailable", ex);
        }
    }

    private record PreparedRequest(
        String kind,
        String operation,
        String method,
        String uri,
        String rawQuery,
        String authHeaderName,
        String authHeaderValue,
        String contentType,
        byte[] body,
        String cacheKey
    ) {}

    private record BoundedRead(byte[] bytes, boolean exceeded) {}

    private record UpstreamResponse(int status, String retryAfterHeader, byte[] body, boolean truncated) {}

    private record ProcessedResponse(int status, byte[] body, Integer retryAfterSeconds, Throwable error) {}

    private record CachedReply(int status, byte[] body) {}

    private static final class PayloadTooLargeException extends RuntimeException {}

    private static final class UpstreamTooLargeException extends IOException {}

    private static final class RetryAfterOnlyHeaderFilterStrategy extends HttpHeaderFilterStrategy {
        @Override
        public boolean applyFilterToExternalHeaders(String headerName, Object headerValue, Exchange exchange) {
            return headerName == null || !"retry-after".equalsIgnoreCase(headerName);
        }
    }

    private static final class ReplyIdempotencyCache {
        private static final int MAX_ENTRIES = 10_000;
        private static final long TTL_NANOS = TimeUnit.MINUTES.toNanos(10);

        private record Entry(int status, byte[] body, long expiresAtNanos) {
            boolean isExpired(long now) {
                return now - expiresAtNanos >= 0;
            }
        }

        private final Object lock = new Object();
        private final LinkedHashMap<String, Entry> completed = new LinkedHashMap<>(256, 0.75f, false) {
            @Override
            protected boolean removeEldestEntry(Map.Entry<String, Entry> eldest) {
                return size() > MAX_ENTRIES;
            }
        };
        private final ConcurrentHashMap<String, CompletableFuture<ProcessedResponse>> inFlight = new ConcurrentHashMap<>();

        CachedReply get(String key) {
            long now = System.nanoTime();
            synchronized (lock) {
                Entry entry = completed.get(key);
                if (entry == null) return null;
                if (entry.isExpired(now)) {
                    completed.remove(key);
                    return null;
                }
                return new CachedReply(entry.status(), entry.body().clone());
            }
        }

        void put(String key, int status, byte[] body) {
            long now = System.nanoTime();
            Entry entry = new Entry(status, body.clone(), now + TTL_NANOS);
            synchronized (lock) {
                Iterator<Map.Entry<String, Entry>> it = completed.entrySet().iterator();
                while (it.hasNext() && it.next().getValue().isExpired(now)) {
                    it.remove();
                }
                completed.remove(key);
                completed.put(key, entry);
            }
        }
    }
}
