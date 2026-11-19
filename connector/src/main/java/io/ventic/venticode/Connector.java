package io.ventic.venticode;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import org.apache.camel.Exchange;
import org.apache.camel.builder.RouteBuilder;
import org.apache.camel.main.Main;

import java.net.URI;
import java.net.URLEncoder;
import java.nio.charset.StandardCharsets;

/** Fixed provider operations reachable only on the Venticode pod's loopback interface. */
public final class Connector extends RouteBuilder {
    private static final ObjectMapper JSON = new ObjectMapper();
    private static final String GITLAB = System.getenv().getOrDefault("GITLAB_ORIGIN", "https://gitlab.com");
    private static final String PORT = System.getenv().getOrDefault("CONNECTOR_PORT", "4190");

    public static void main(String[] args) throws Exception {
        URI gitlab = URI.create(GITLAB);
        if (!"https".equals(gitlab.getScheme()) || gitlab.getHost() == null || gitlab.getUserInfo() != null
                || !gitlab.getRawPath().isEmpty() || gitlab.getQuery() != null || gitlab.getFragment() != null)
            throw new IllegalArgumentException("GITLAB_ORIGIN must be a fixed HTTPS origin");
        int port = Integer.parseInt(PORT);
        if (port < 1 || port > 65535)
            throw new IllegalArgumentException("CONNECTOR_PORT must be between 1 and 65535");
        Main main = new Main();
        main.configure().addRoutesBuilder(new Connector());
        main.run();
    }

    @Override public void configure() {
        // Provider responses can contain tokens. Never log exchange bodies, exceptions or request URIs.
        var https = getContext().getComponent("https", org.apache.camel.component.http.HttpComponent.class);
        https.setAutomaticRetriesDisabled(true);
        https.setRedirectHandlingDisabled(true);
        errorHandler(defaultErrorHandler().maximumRedeliveries(0).logExhausted(false).logStackTrace(false));
        onException(IllegalArgumentException.class).handled(true).removeHeaders("*")
            .setHeader(Exchange.CONTENT_TYPE, constant("application/json"))
            .setHeader(Exchange.HTTP_RESPONSE_CODE, constant(400))
            .setBody(constant("{\"error\":\"Invalid connector request\"}"));
        onException(Exception.class).handled(true).removeHeaders("*")
            .setHeader(Exchange.CONTENT_TYPE, constant("application/json"))
            .setHeader(Exchange.HTTP_RESPONSE_CODE, constant(502))
            .setBody(constant("{\"error\":\"Connector request failed\"}"));
        from("jetty:http://127.0.0.1:" + PORT + "/healthz?httpMethodRestrict=GET").routeId("healthz")
            .setHeader(Exchange.CONTENT_TYPE, constant("text/plain"))
            .setHeader(Exchange.HTTP_RESPONSE_CODE, constant(200))
            .setBody(constant("ok"));
        from("jetty:http://127.0.0.1:" + PORT + "/exchange?httpMethodRestrict=POST").routeId("provider-exchange")
            .process(this::prepare)
            .to("https://localhost?throwExceptionOnFailure=false&followRedirects=false&skipResponseHeaders=true&connectTimeout=5000&connectionRequestTimeout=5000&responseTimeout=15000")
            .convertBodyTo(String.class)
            .process(e -> {
                int status = e.getMessage().getHeader(Exchange.HTTP_RESPONSE_CODE, 502, Integer.class);
                String response = e.getMessage().getBody(String.class);
                if ("remove".equals(e.getProperty("operation")) && status == 404) {
                    e.getMessage().removeHeaders("*");
                    e.getMessage().setHeader(Exchange.HTTP_RESPONSE_CODE, 200);
                    e.getMessage().setHeader(Exchange.CONTENT_TYPE, "application/json");
                    e.getMessage().setBody(JSON.writeValueAsBytes(JSON.createObjectNode().put("ok", true)));
                    return;
                }
                JsonNode result = response == null || response.isBlank() ? JSON.createObjectNode() : JSON.readTree(response);
                if ("remove".equals(e.getProperty("operation")) &&
                        (result.path("error").asText().equals("token_revoked") || result.path("error").asText().equals("invalid_auth"))) {
                    status = 200;
                    result = JSON.createObjectNode().put("ok", true);
                }
                if (status < 200 || status >= 300 || result == null || !result.path("ok").asBoolean(true)) {
                    int outStatus = (status == 401 || status == 403 || status == 429) ? status : 502;
                    e.getMessage().removeHeaders("*");
                    e.getMessage().setHeader(Exchange.HTTP_RESPONSE_CODE, outStatus);
                    e.getMessage().setHeader(Exchange.CONTENT_TYPE, "application/json");
                    e.getMessage().setBody("{\"error\":\"Provider rejected operation\"}".getBytes(StandardCharsets.UTF_8));
                    return;
                }
                e.getMessage().removeHeaders("*");
                e.getMessage().setHeader(Exchange.CONTENT_TYPE, "application/json");
                e.getMessage().setBody(JSON.writeValueAsBytes(result));
            });
    }

    private static String field(JsonNode n, String key) {
        String value = n.path(key).asText("");
        if (value.isEmpty() || value.length() > 65536) throw new IllegalArgumentException("Missing connector field");
        return value;
    }
    private static String httpsUrl(JsonNode n, String key) {
        String value = field(n, key);
        URI uri = URI.create(value);
        if (!"https".equals(uri.getScheme()) || uri.getHost() == null || uri.getUserInfo() != null)
            throw new IllegalArgumentException("HTTPS URL required");
        return value;
    }
    private static String numeric(JsonNode n, String key) {
        String value = field(n,key);
        if (!value.matches("[0-9]{1,20}")) throw new IllegalArgumentException("Invalid number");
        return value;
    }
    private static String encoded(String s) { return URLEncoder.encode(s, StandardCharsets.UTF_8); }

    private void prepare(Exchange e) throws Exception {
        JsonNode req = JSON.readTree(e.getMessage().getBody(byte[].class));
        JsonNode c = req.path("credentials");
        String op = field(req,"operation");
        e.setProperty("operation", op);
        String method = "POST";
        String url;
        ObjectNode body = JSON.createObjectNode();
        String form = null;
        e.getMessage().removeHeaders("*");
        switch (field(req,"kind")) {
            case "slack" -> {
                url = "https://slack.com/api/";
                switch (op) {
                    case "exchange" -> {
                        url += "oauth.v2.access";
                        form = "client_id=" + encoded(field(c,"client_id")) + "&client_secret=" + encoded(field(c,"client_secret"))
                            + "&code=" + encoded(field(req,"code")) + "&redirect_uri=" + encoded(field(req,"redirect_uri"));
                    }
                    case "identify" -> url += "auth.test";
                    case "remove" -> url += "auth.revoke";
                    case "inspect" -> {url += "conversations.info?channel=" + encoded(field(req,"destination"));method="GET";}
                    case "reply" -> {
                        url += "chat.postMessage";
                        body.put("channel",field(req,"destination")).put("thread_ts",field(req,"thread")).put("text",field(req,"text"));
                    }
                    default -> throw new IllegalArgumentException("Unknown Slack operation");
                }
                if (!op.equals("exchange")) e.getMessage().setHeader("Authorization","Bearer " + field(c,"token"));
            }
            case "telegram" -> {
                String token = field(c,"token");
                if (!token.matches("[0-9]+:[A-Za-z0-9_-]+")) throw new IllegalArgumentException("Invalid Telegram identity");
                url = "https://api.telegram.org/bot" + token + "/";
                switch (op) {
                    case "identify" -> url += "getMe";
                    case "inspect" -> {url += "getChat";body.put("chat_id",field(req,"destination"));}
                    case "install" -> {url += "setWebhook";body.put("url",httpsUrl(req,"webhook_url")).put("secret_token",field(c,"webhook_secret"));body.putArray("allowed_updates").add("message");}
                    case "remove" -> url += "deleteWebhook";
                    case "reply" -> {url += "sendMessage";body.put("chat_id",field(req,"destination")).put("text",field(req,"text"))
                        .putObject("reply_parameters").put("message_id",Long.parseLong(numeric(req,"thread")));}
                    default -> throw new IllegalArgumentException("Unknown Telegram operation");
                }
            }
            case "gitlab" -> {
                e.getMessage().setHeader("PRIVATE-TOKEN",field(c,"token"));
                if ("identify".equals(op)) {
                    url = GITLAB + "/api/v4/user";
                    method = "GET";
                } else {
                    url = GITLAB + "/api/v4/projects/" + numeric(req,"destination");
                    switch (op) {
                        case "inspect" -> method="GET";
                        case "hooks" -> {url += "/hooks?per_page=100&page=" + numeric(req,"page");method="GET";}
                        case "install" -> {
                            url += "/hooks";
                            JsonNode hookId = req.path("hook_id");
                            if (!hookId.isMissingNode() && !hookId.isNull()) {url += "/"+numeric(req,"hook_id");method="PUT";}
                            body.put("url",httpsUrl(req,"webhook_url")).put("token",field(c,"webhook_secret"))
                                .put("note_events",true).put("push_events",false).put("enable_ssl_verification",true);
                        }
                        case "remove" -> {url += "/hooks/"+numeric(req,"hook_id");method="DELETE";}
                        case "reply" -> {
                            String type=field(req,"object_type");
                            if (!type.equals("issues") && !type.equals("merge_requests")) throw new IllegalArgumentException("Unknown GitLab object");
                            url += "/"+type+"/"+numeric(req,"iid");
                            String thread=field(req,"thread");
                            if (!thread.equals("root") && !thread.matches("[a-zA-Z0-9_-]{1,128}")) throw new IllegalArgumentException("Invalid GitLab discussion");
                            url += thread.equals("root") ? "/notes" : "/discussions/"+thread+"/notes";
                            body.put("body",field(req,"text"));
                        }
                        default -> throw new IllegalArgumentException("Unknown GitLab operation");
                    }
                }
            }
            default -> throw new IllegalArgumentException("Unknown provider");
        }
        URI target = URI.create(url);
        e.getMessage().setHeader(Exchange.HTTP_URI, url.split("\\?", 2)[0]);
        e.getMessage().setHeader(Exchange.HTTP_QUERY, target.getRawQuery());
        e.getMessage().setHeader(Exchange.HTTP_METHOD,method);
        boolean hasBody = method.equals("POST") || method.equals("PUT");
        if (hasBody) {
            e.getMessage().setHeader(Exchange.CONTENT_TYPE, form == null ? "application/json" : "application/x-www-form-urlencoded");
            e.getMessage().setBody(form == null ? JSON.writeValueAsBytes(body) : form.getBytes(StandardCharsets.UTF_8));
        } else {
            e.getMessage().setBody(null);
        }
    }
}
