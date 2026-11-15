package io.github.cepheus.app.net

import io.github.cepheus.app.pair.PairingTarget
import io.github.cepheus.app.protocol.Protocol
import io.github.cepheus.app.protocol.Requests
import io.github.cepheus.app.protocol.ServerMessage
import io.github.cepheus.app.protocol.rid
import io.github.cepheus.app.store.Credentials
import io.github.cepheus.app.upload.Sent
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString.Companion.toByteString
import java.io.IOException
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import java.security.cert.CertificateException
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import javax.net.ssl.SSLContext
import javax.net.ssl.SSLException
import javax.net.ssl.SSLPeerUnverifiedException
import kotlinx.coroutines.currentCoroutineContext

/**
 * A failure the owner can read: the gateway's own `err` text, or what went
 * wrong reaching it. `code` is the gateway's kind of refusal, when it names one.
 */
class DeckException(message: String, val code: String? = null) : Exception(message)

/** Where the connection to the deck stands. */
sealed interface Link {
    data object Unpaired : Link
    data object Connecting : Link
    data object Online : Link

    /** Out of reach; the next try is at `retryAt` (ms). */
    data class Offline(val reason: String, val retryAt: Long) : Link

    /** The gateway refused this phone; it waits for the owner rather than retry into a lockout. */
    data class Denied(val reason: String) : Link
}

/**
 * One WebSocket to the deck's gateway, kept open while [start]ed: it signs
 * in on every connect, pings every 30 seconds, and reconnects with [Backoff]
 * or at once when the network changes ([kick]). The gateway pings too, so a
 * sleeping phone whose own timers stop still answers and stays connected.
 */
class GatewayClient(
    private val scope: CoroutineScope,
    private val credentials: () -> Credentials?,
    private val alertsHeld: () -> Pair<Long?, String?>,
    private val onLink: (Link) -> Unit,
    private val onMessage: (ServerMessage) -> Unit,
    private val timing: Timing = Timing(),
) {
    /** How long a request waits for its answer, and how often and how long the app pings; tests shorten them. */
    data class Timing(
        val requestMs: Long = REQUEST_TIMEOUT_MS,
        val pingEveryMs: Long = PING_EVERY_MS,
        val pingTimeoutMs: Long = PING_TIMEOUT_MS,
    )

    private val backoff = Backoff()
    private val kicks = Channel<Unit>(Channel.CONFLATED)
    private val nextRid = AtomicLong(1)
    private val nextConnection = AtomicLong(0)
    @Volatile private var activeConnection = 0L
    @Volatile private var activeRun = 0L
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<ServerMessage>>()
    private var job: Job? = null

    @Volatile
    private var socket: WebSocket? = null

    /** When the gateway last sent a message ([System.nanoTime]). */
    @Volatile
    private var heardAt = 0L

    @Volatile
    var online = false
        private set

    @Synchronized
    fun start() {
        if (job?.isActive == true) return
        val run = nextConnection.incrementAndGet()
        activeRun = run
        job = scope.launch { run(run) }
    }

    @Synchronized
    fun stop() {
        activeConnection = nextConnection.incrementAndGet()
        activeRun = activeConnection
        online = false
        socket?.cancel()
        socket = null
        fail("The connection to the deck was lost.")
        job?.cancel()
        job = null
    }

    /** The network changed: test a live connection, or end the wait before the next try. */
    fun kick() {
        if (online) {
            scope.launch {
                try {
                    request(timing.pingTimeoutMs) { Requests.ping(it) }
                } catch (_: DeckException) {
                    socket?.cancel()
                }
            }
        } else {
            kicks.trySend(Unit)
        }
    }

    /** Sends a request built with its `rid`; the reply, or [DeckException] with the gateway's error. */
    suspend fun request(timeoutMs: Long = timing.requestMs, build: (Long) -> String): ServerMessage =
        exchange(timeoutMs) { ws, rid -> ws.send(build(rid)) }

    /**
     * As [request], for a binary frame (a piece of a file), sent before this
     * returns and after every frame sent before it, so several can be on
     * their way at once; the answer is due `timeoutMs` after sending.
     */
    fun sendBinary(timeoutMs: Long = timing.requestMs, build: (Long) -> ByteArray): Sent {
        val (rid, waiter) = post { ws, rid -> ws.send(build(rid).toByteString()) }
        val due = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMs)
        return Sent { answer(rid, waiter, TimeUnit.NANOSECONDS.toMillis(due - System.nanoTime())) }
    }

    private suspend fun exchange(timeoutMs: Long, send: (WebSocket, Long) -> Boolean): ServerMessage {
        val (rid, waiter) = post(send)
        return answer(rid, waiter, timeoutMs)
    }

    /** Send a request now; its `rid` and where its answer will arrive. */
    private fun post(send: (WebSocket, Long) -> Boolean): Pair<Long, CompletableDeferred<ServerMessage>> {
        val ws = socket?.takeIf { online } ?: throw DeckException("Not connected to the deck.")
        val rid = nextRid.getAndIncrement()
        val waiter = CompletableDeferred<ServerMessage>()
        pending[rid] = waiter
        try {
            if (!send(ws, rid)) throw DeckException("Not connected to the deck.")
        } catch (error: Throwable) {
            pending.remove(rid)
            throw error
        }
        return rid to waiter
    }

    private suspend fun answer(rid: Long, waiter: CompletableDeferred<ServerMessage>, timeoutMs: Long): ServerMessage {
        val reply = try {
            withTimeout(timeoutMs.coerceAtLeast(1)) { waiter.await() }
        } catch (_: TimeoutCancellationException) {
            pending.remove(rid)
            throw DeckException("The deck did not answer in time.")
        }
        if (reply is ServerMessage.Err) throw DeckException(reply.error, reply.code)
        return reply
    }

    private suspend fun run(run: Long) {
        while (currentCoroutineContext().isActive && activeRun == run) {
            val creds = credentials()
            if (creds == null) {
                onLink(Link.Unpaired)
                return
            }
            onLink(Link.Connecting)
            val end = connect(creds, run)
            if (activeRun != run) return
            if (end.denied != null) {
                onLink(Link.Denied(end.denied))
                return
            }
            if (end.signedIn) backoff.reset()
            val wait = backoff.next()
            onLink(Link.Offline(end.error ?: "The connection closed.", System.currentTimeMillis() + wait))
            withTimeoutOrNull(wait) { kicks.receive() }
        }
    }

    private class End(val signedIn: Boolean, val denied: String?, val error: String?)

    private suspend fun connect(creds: Credentials, run: Long): End {
        val generation = synchronized(this) {
            if (activeRun != run) return End(false, null, null)
            nextConnection.incrementAndGet().also { activeConnection = it }
        }
        val closed = CompletableDeferred<End>()
        val http = client(creds.fingerprint)
        var signedIn = false
        val listener = object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                if (activeConnection != generation) { webSocket.cancel(); return }
                heardAt = System.nanoTime()
                val (lastSeq, epoch) = alertsHeld()
                webSocket.send(Requests.auth(creds.device, creds.key, lastSeq, epoch))
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                if (activeConnection != generation) return
                heardAt = System.nanoTime()
                when (val msg = Protocol.parse(text)) {
                    is ServerMessage.Denied -> {
                        closed.complete(End(signedIn, msg.reason, null))
                        webSocket.close(1000, null)
                    }
                    is ServerMessage.Hello -> {
                        signedIn = true
                        onMessage(msg)
                    }
                    is ServerMessage.State -> {
                        if (!signedIn) return
                        online = msg.state.ready
                        if (!online) fail("The Cepheus service is not ready; waiting for its session state.")
                        onLink(if (online) Link.Online else Link.Connecting)
                        onMessage(msg)
                    }
                    else -> {
                        val waiter = msg.rid?.let { pending.remove(it) }
                        if (waiter != null) waiter.complete(msg) else onMessage(msg)
                    }
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                if (activeConnection != generation) return
                webSocket.close(1000, null)
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                if (activeConnection != generation) return
                closed.complete(End(signedIn, null, null))
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                if (activeConnection != generation) return
                closed.complete(End(signedIn, null, describe(t, creds.host, creds.port)))
            }
        }
        val ws = http.newWebSocket(Request.Builder().url(creds.url).build(), listener)
        synchronized(this) {
            if (activeRun == run && activeConnection == generation) socket = ws else ws.cancel()
        }
        val pinger = scope.launch {
            while (true) {
                delay(timing.pingEveryMs)
                if (!online) continue
                val asked = System.nanoTime()
                try {
                    request(timing.pingTimeoutMs) { Requests.ping(it) }
                } catch (_: DeckException) {
                    // The answer waits behind whatever the phone sent first, such as a

                    // sticks to 0 and fails"). Anything the deck sent since the ping went
                    // shows the connection lives; a silent one is left.
                    if (heardAt - asked <= 0) ws.cancel()
                }
            }
        }
        try {
            return closed.await()
        } finally {
            pinger.cancel()
            ws.cancel()
            synchronized(this) {
                if (activeConnection == generation) {
                    activeConnection = nextConnection.incrementAndGet()
                    online = false
                    socket = null
                    fail("The connection to the deck was lost.")
                }
            }
        }
    }

    private fun fail(reason: String) {
        val waiters = pending.values.toList()
        pending.clear()
        waiters.forEach { it.completeExceptionally(DeckException(reason)) }
    }

    private var http: Pair<String, OkHttpClient>? = null

    @Synchronized
    private fun client(fingerprint: String): OkHttpClient {
        http?.let { (fp, c) -> if (fp == fingerprint) return c }
        return pinnedClient(fingerprint).also { http = fingerprint to it }
    }

    companion object {
        const val REQUEST_TIMEOUT_MS = 30_000L

        /** `new` and `model` wait until the program is ready. */
        const val SLOW_TIMEOUT_MS = 120_000L
        private const val PING_EVERY_MS = 30_000L
        private const val PING_TIMEOUT_MS = 15_000L

        fun pinnedClient(fingerprint: String): OkHttpClient {
            val trust = PinnedTrustManager(fingerprint)
            val tls = SSLContext.getInstance("TLS").apply { init(null, arrayOf(trust), null) }
            return OkHttpClient.Builder()
                .sslSocketFactory(tls.socketFactory, trust)
                .hostnameVerifier(PinnedHostnameVerifier(fingerprint))
                // WebSocket pings too: any frame keeps the gateway's 90-second idle limit away,
                // and a missing pong ends a connection the network silently dropped.
                .pingInterval(PING_EVERY_MS, TimeUnit.MILLISECONDS)
                .connectTimeout(10, TimeUnit.SECONDS)
                .readTimeout(0, TimeUnit.MILLISECONDS)
                .retryOnConnectionFailure(false)
                .build()
        }

        /** Pair with the code the deck shows; the reply is this phone's sign-in. */
        suspend fun pair(target: PairingTarget, name: String): Credentials {
            val result = CompletableDeferred<Credentials>()
            val http = pinnedClient(target.fingerprint)
            val ws = http.newWebSocket(
                Request.Builder().url(target.url).build(),
                object : WebSocketListener() {
                    override fun onOpen(webSocket: WebSocket, response: Response) {
                        webSocket.send(Requests.pair(target.code, name))
                    }

                    override fun onMessage(webSocket: WebSocket, text: String) {
                        when (val msg = Protocol.parse(text)) {
                            is ServerMessage.Paired -> {
                                result.complete(Credentials(target.host, target.port, target.fingerprint, msg.device, msg.key))
                                webSocket.close(1000, null)
                            }
                            is ServerMessage.Denied -> {
                                result.completeExceptionally(DeckException(msg.reason))
                                webSocket.close(1000, null)
                            }
                            else -> {}
                        }
                    }

                    override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                        result.completeExceptionally(DeckException(describe(t, target.host, target.port)))
                    }

                    override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                        result.completeExceptionally(DeckException("The deck closed the connection."))
                    }
                },
            )
            try {
                return withTimeout(20_000) { result.await() }
            } catch (_: TimeoutCancellationException) {
                throw DeckException("The deck did not answer in time.")
            } finally {
                ws.cancel()
                http.dispatcher.executorService.shutdown()
                http.connectionPool.evictAll()
            }
        }

        fun describe(t: Throwable, host: String, port: Int): String {
            var cause: Throwable? = t
            while (cause != null) {
                when (cause) {
                    is CertificateException, is SSLPeerUnverifiedException ->
                        return "The deck's certificate is not the one paired. Pair again if the deck was reinstalled."
                    is UnknownHostException, is ConnectException, is NoRouteToHostException ->
                        return "Can't reach $host:$port. Is the VPN on?"
                    is SocketTimeoutException -> return "$host:$port did not answer."
                }
                cause = cause.cause
            }
            return when (t) {
                is SSLException -> "The secure connection failed: ${t.message}"
                is IOException -> t.message ?: "The connection failed."
                else -> t.message ?: t.javaClass.simpleName
            }
        }
    }
}
