package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.Item
import io.github.cepheus.app.protocol.Status
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class OutboxTest {
    @Test fun claudeSubmissionWaitsForCanonicalHistoryWithoutExpiringOrPromotingItself() {
        val original = out("original owner input", delivered = false, queued = true).copy(tokens = listOf("token"), uris = listOf("content://original"))
        val submitted = Outbox.acknowledged(original, "claude", sent + 1)
        assertFalse(submitted.delivered)
        assertTrue(submitted.submitted)
        assertFalse(submitted.queued)
        assertFalse(Outbox.offersSendNow(submitted))
        assertFalse(Outbox.thinking(Status.IDLE, true, listOf(submitted)))
        assertEquals(listOf(submitted), Outbox.settle(listOf(submitted), view(*filler(10)), sent, sent + 100 * Outbox.SETTLE_MS))
        assertEquals("canonical history replaces the optimistic submission", emptyList<Outgoing>(),
            Outbox.settle(listOf(submitted), view(*filler(10), Item.User(original.text, null)), sent, sent + 100 * Outbox.SETTLE_MS))
        assertEquals("an earlier equal message cannot acknowledge a new submission", listOf(submitted),
            Outbox.settle(listOf(submitted), view(Item.User(original.text, null), *filler(10)), sent, sent + 100 * Outbox.SETTLE_MS))
        assertEquals("terminal output is not canonical history", listOf(submitted),
            Outbox.settle(listOf(submitted), view(Item.Screen(original.text), source = "screen"), sent, sent + 100 * Outbox.SETTLE_MS))
        assertEquals(original.tokens, submitted.tokens)
        assertEquals(original.uris, submitted.uris)
    }

    @Test fun submittedTransportSurvivesRestartWithoutAResendOrFalseFailure() {
        val values = mutableMapOf<String, String>()
        val storage = object : KeyValue {
            override fun getString(key: String) = values[key]
            override fun putString(key: String, value: String?) { if (value == null) values.remove(key) else values[key] = value }
            override fun getLong(key: String) = values[key]?.toLongOrNull()
            override fun putLong(key: String, value: Long?) = putString(key, value?.toString())
        }
        val submitted = Outbox.acknowledged(out("original", delivered = false, paths = listOf("/private/file"))
            .copy(tokens = listOf("upload-token"), uris = listOf("content://original")), "claude", sent, hostAt = sent - 2_000)
        val id = (3L shl 40) or 7
        OutboxDrafts(storage).save(mapOf(id to listOf(submitted)))
        val restored = OutboxDrafts(storage).load().getValue(id).single()
        assertEquals(submitted, restored)
        assertFalse(Outbox.offersSendNow(restored))
        assertEquals(listOf(restored), Outbox.settle(listOf(restored), null, sent, sent + 100 * Outbox.SETTLE_MS))
        // Older uncertain records retain their existing explicit recovery state.
        OutboxDrafts(storage).save(mapOf(7L to listOf(out("uncertain", delivered = false))))
        assertTrue(OutboxDrafts(storage).load().getValue(7).single().error!!.contains("not confirmed"))
        assertFalse(OutboxDrafts(storage).load().getValue(7).single().knownUnsent)
        val refused = out("refused", delivered = false).copy(error = "The session is gone", knownUnsent = true)
        OutboxDrafts(storage).save(mapOf(7L to listOf(refused)))
        assertEquals(refused, OutboxDrafts(storage).load().getValue(7).single())
    }

    @Test fun aMessageRefusedBeforeSendingIsNeverTakenForAnEqualLoggedOne() {
        val refused = out("yes", delivered = false).copy(error = "This session's identity changed", knownUnsent = true)
        assertEquals(listOf(refused), Outbox.settle(listOf(refused), view(*filler(10), Item.User("yes", null)), null, sent))
        assertEquals(listOf(refused), Outbox.settle(listOf(refused), view(Item.User("yes", null), generation = 2), null, sent))
        assertEquals(listOf(refused), Outbox.settle(listOf(refused), view(*filler(10)), sent, sent + 100 * Outbox.SETTLE_MS))
    }






    @Test fun aMessageClaudeDropsAfterTheDeckTookItShowsWhyOnItsOwnCopy() {
        val dropped = Outbox.acknowledged(out("go", delivered = false).copy(clientId = "this-send"), "claude", sent + 1)
        val other = Outbox.acknowledged(out("go", delivered = false).copy(clientId = "another-send"), "claude", sent + 2)
        val error = "Claude did not take the message (the prompt was removed from the queue before it ran); nothing was sent"
        val marked = Outbox.refused(listOf(dropped, other), "this-send", error)
        val refused = marked[0]
        assertEquals(error, refused.error)
        assertTrue(refused.knownUnsent)
        assertFalse(refused.submitted)
        assertEquals(dropped.text, refused.text)
        assertEquals("another send of the same words keeps its own state", other, marked[1])
        assertEquals("a later equal message in the log is not the dropped one", listOf(refused),
            Outbox.settle(listOf(refused), view(*filler(10), Item.User("go", null)), sent, sent + 100 * Outbox.SETTLE_MS))
    }

    @Test
    fun originalSendTimeSurvivesALateDeliveryAcknowledgement() {
        val original = Outgoing(1, "A queued message", emptyList(), emptyList(), 0, 0, 1000, false, true)
        val delivered = original.copy(delivered = true, sentAt = 100_000)
        assertEquals(1000L, delivered.createdAt)
    }

    @Test fun nativeReceiptsSurviveRestartAndReconcileByIdentityWithPictures() {
        val values = mutableMapOf<String, String>()
        val storage = object : KeyValue {
            override fun getString(key: String) = values[key]
            override fun putString(key: String, value: String?) { if (value == null) values.remove(key) else values[key] = value }
            override fun getLong(key: String) = values[key]?.toLongOrNull()
            override fun putLong(key: String, value: Long?) = putString(key, value?.toString())
        }
        for (receipt in listOf("queued", "steered", "started", "submitted")) {
            val original = out("look", paths = listOf("/uploads/photo.png"), delivered = false)
                .copy(clientId = "this-send", correlated = true, tokens = listOf("token"), uris = listOf("content://photo"))
            val accepted = Outbox.acknowledged(original, "codex", sent + 1, receipt)
            OutboxDrafts(storage).save(mapOf(7L to listOf(accepted)))
            val restored = OutboxDrafts(storage).load().getValue(7).single()
            assertEquals(accepted, restored)
            assertEquals(listOf(restored), Outbox.settle(listOf(restored), null, sent, sent + 100 * Outbox.SETTLE_MS))
            val other = Item.User("look", sent, listOf("other-image"), "another-send")
            assertEquals(listOf(restored), Outbox.settle(listOf(restored), view(*filler(10), other), null, sent))
            // Exact client identity survives rewritten indexes and a lost acknowledgement.
            val logged = Item.User("look", sent + 2, listOf("native-image"), "this-send")
            assertTrue(Outbox.settle(listOf(restored.copy(error = "timeout")), view(logged), null, sent).isEmpty())
        }
        val legacy = out("look", paths = listOf("/uploads/photo.png"))
        // A matching caption and image count cannot prove the attachment identity.
        assertEquals(listOf(legacy), Outbox.settle(listOf(legacy), view(*filler(10), Item.User("look", sent + 1, listOf("native-image"))), null, sent))
    }

    private val sent = 1_000_000L

    private fun out(
        text: String,
        key: Long = 1,
        after: Int = 10,
        generation: Long = 1,
        delivered: Boolean = true,
        paths: List<String> = emptyList(),
        queued: Boolean = false,
    ) = Outgoing(key, text, paths.map { it.substringAfterLast('/') }, paths, generation, after, sent, delivered, queued)

    /** A chat holding `items` from `start` on, as the gateway sent them. */
    private fun view(vararg items: Item, start: Int = 0, generation: Long = 1, source: String = "log") =
        ChatView(7, start = start, total = start + items.size, generation = generation, source = source, items = items.toList(), live = true)

    private fun filler(n: Int) = Array<Item>(n) { Item.Assistant("reply $it", null) }

    @Test
    fun aSentMessageWaitsUntilTheLogGainsIt() {
        val original = out("hello")
        // A timeout or restart does not outweigh the conversation's later receipt.
        for (message in listOf(original, original.copy(delivered = false, error = "Acknowledgement timed out"),
            original.copy(error = "Delivery was not confirmed after restart"))) {
            val pending = listOf(message)
            val before = view(*filler(10))
            assertEquals(pending, Outbox.settle(pending, before, quietSince = null, now = sent + 5_000))
            if (message.error != null) {
                assertEquals(pending, Outbox.settle(pending, before, quietSince = sent, now = sent + 10 * Outbox.SETTLE_MS))
            }
            val logged = view(*filler(10), Item.User("hello", null))
            assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, logged, quietSince = null, now = sent + 5_000))
        }
    }

    /** "yes" said before the message was sent is not this "yes". */
    @Test
    fun anEarlierMessageWithTheSameTextDoesNotCount() {
        val pending = listOf(out("yes", after = 3))
        val earlier = view(Item.User("yes", null), *filler(3))
        assertEquals(pending, Outbox.settle(pending, earlier, quietSince = null, now = sent))
    }

    @Test
    fun eachLoggedMessageTakesOneSentMessageInOrder() {
        val pending = listOf(out("yes", key = 1), out("yes", key = 2), out("go on", key = 3))
        val logged = view(*filler(10), Item.User("yes", null))
        for (messages in listOf(pending, pending.map { Outbox.acknowledged(it, "claude", sent) })) {
            assertEquals(listOf(2L, 3L), Outbox.settle(messages, logged, quietSince = null, now = sent).map { it.key })
        }
    }

    /** The log may break lines differently, and a message with files carries their paths after it. */
    @Test
    fun lineBreaksAndAttachedPathsStillMatch() {
        val pending = listOf(out("fix\r\nthis  please", paths = listOf("/u/7/shot.png", "/u/7/log.txt")))
        val logged = view(*filler(10), Item.User("fix\nthis please\n\n/u/7/shot.png\n/u/7/log.txt", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, logged, quietSince = null, now = sent))

        val filesOnly = listOf(out("", paths = listOf("/u/7/shot.png")))
        val other = view(*filler(10), Item.User("something typed at the desk", null))
        assertEquals(filesOnly, Outbox.settle(filesOnly, other, quietSince = null, now = sent))
        val itsPath = view(*filler(10), Item.User("/u/7/shot.png", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(filesOnly, itsPath, quietSince = null, now = sent))

        // A session on another host is sent that host's copy of each file,
        // and its log names the copy, which the gateway's receipt gives.
        val elsewhere = "/srv/peer/uploads/shot.png"
        val remote = Outbox.acknowledged(out("look", paths = listOf("/u/7/shot.png"), delivered = false), "claude", sent, "submitted",
            hostAt = sent, paths = listOf(elsewhere))
        val remoteLog = view(*filler(10), Item.User("look\n\n$elsewhere", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(listOf(remote), remoteLog, quietSince = null, now = sent))
    }








    @Test
    fun aMessageWhoseProviderTookItsPicturesStillMatches() {
        val shot = listOf("/u/67/Screenshot.jpg")
        val words = "and it stays listed as a ghost session when its ended"
        val pending = listOf(out(words, paths = shot))
        // Taken during a turn: one record, its picture a part of it.
        val queued = view(*filler(10), Item.User("[Image #4]$words\n[image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, queued, quietSince = null, now = sent))
        // Taken at once: the picture is an item of its own.
        val direct = view(*filler(10), Item.User("[Image #3]$words", null), Item.User("[image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, direct, quietSince = null, now = sent))
        // The words must still be the message's.
        val otherWords = view(*filler(10), Item.User("[Image #4]something typed at the desk\n[image]", null))
        assertEquals(pending, Outbox.settle(pending, otherWords, quietSince = null, now = sent))

        val alone = listOf(out("", paths = shot))
        val itsPicture = view(*filler(10), Item.User("[Image #1]", null), Item.User("[image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(alone, itsPicture, quietSince = null, now = sent))
        val queuedPicture = view(*filler(10), Item.User("[Image #1]\n[image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(alone, queuedPicture, quietSince = null, now = sent))
        // A picture with words typed at the desk, or the picture part of
        // another message, is not a picture sent alone.
        val withWords = view(*filler(10), Item.User("[Image #1]a picture from the desk", null), Item.User("[image]", null))
        assertEquals(alone, Outbox.settle(alone, withWords, quietSince = null, now = sent))

        // Another file keeps its path in the log.
        val mixed = listOf(out("fix this", paths = listOf("/u/7/shot.png", "/u/7/log.txt")))
        val mixedLog = view(*filler(10), Item.User("[Image #2]fix this\n\n/u/7/log.txt\n[image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(mixed, mixedLog, quietSince = null, now = sent))
        val pathsOnly = listOf(out("", paths = listOf("/u/7/shot.png", "/u/7/log.txt")))
        val pathsLog = view(*filler(10), Item.User("[Image #2]\n/u/7/log.txt\n[image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pathsOnly, pathsLog, quietSince = null, now = sent))

        // Dot publishes one exact attachment suffix per uploaded native filename.
        // The upload can sanitize/rename the display name before the provider sees it.
        val dot = out("look at this", paths = listOf("/u/7/Screen_shot-2.png")).copy(files = listOf("Screen shot.png"))
        val dotLog = view(*filler(10), Item.User("look at this\n[Image: Screen_shot-2.png]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(listOf(dot), dotLog, null, sent))
        val dotFallback = view(*filler(10), Item.User("look at this\n[Image: image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(listOf(dot), dotFallback, null, sent))
        assertEquals(listOf(2L), Outbox.settle(listOf(dot, dot.copy(key = 2)), dotLog, null, sent).map { it.key })
        val dotOnly = dot.copy(text = "")
        val dotPicture = view(*filler(10), Item.User("\n[Image: Screen_shot-2.png]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(listOf(dotOnly), dotPicture, null, sent))
        val pair = dot.copy(paths = listOf("/u/7/one.png", "/u/7/two.jpg"))
        val mixedNames = view(*filler(10), Item.User("look at this\n[Image: one.png]\n[Image: image]", null))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(listOf(pair), mixedNames, null, sent))
        val wrongOrder = view(*filler(10), Item.User("look at this\n[Image: two.jpg]\n[Image: one.png]", null))
        assertEquals(listOf(pair), Outbox.settle(listOf(pair), wrongOrder, null, sent))
        val otherPicture = view(*filler(10), Item.User("look at this\n[Image: another.png]", null))
        assertEquals(listOf(dot), Outbox.settle(listOf(dot), otherPicture, null, sent))
        val inline = view(*filler(10), Item.User("look at this [Image: Screen_shot-2.png]", null))
        assertEquals(listOf(dot), Outbox.settle(listOf(dot), inline, null, sent))
        val extraPicture = view(*filler(10), Item.User("look at this\n[Image: another.png]\n[Image: Screen_shot-2.png]", null))
        assertEquals(listOf(dot), Outbox.settle(listOf(dot), extraPicture, null, sent))
        val uncertain = dot.copy(delivered = false, error = "Upload/send was not confirmed", tokens = listOf("kept-token"), uris = listOf("content://original"))
        assertEquals(listOf(uncertain), Outbox.settle(listOf(uncertain), dotLog, sent, sent + 100 * Outbox.SETTLE_MS))
        assertEquals(listOf(uncertain), Outbox.settle(listOf(uncertain), dotFallback, sent, sent + 100 * Outbox.SETTLE_MS))
    }

    /** Claude logs a message typed during a turn only when the turn reaches it, minutes later. */
    @Test
    fun aMessageWaitsAsLongAsTheSessionIsBusy() {
        val pending = listOf(out("and then this"))
        assertEquals(pending, Outbox.settle(pending, view(*filler(10)), quietSince = null, now = sent + 600_000))
    }

    @Test
    fun aTakenMessageNoLogShowsLeavesOnceTheSessionIsQuiet() {
        val pending = listOf(out("/help"))
        val quiet = sent + 60_000
        val chat = view(*filler(10))
        assertEquals(pending, Outbox.settle(pending, chat, quiet, now = quiet + Outbox.SETTLE_MS - 1))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, chat, quiet, now = quiet + Outbox.SETTLE_MS))
        // Quiet before it was taken: counted from when it was taken.
        assertEquals(pending, Outbox.settle(pending, chat, sent - 60_000, now = sent + Outbox.SETTLE_MS - 1))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, chat, sent - 60_000, now = sent + Outbox.SETTLE_MS))
    }

    @Test
    fun aMessageStillBeingSentNeverLeavesByTime() {
        val pending = listOf(out("hello", delivered = false))
        assertEquals(pending, Outbox.settle(pending, view(*filler(10)), sent, now = sent + 10 * Outbox.SETTLE_MS))
    }

    /** `/clear` starts another chat: its log starts again from the first item. */
    @Test
    fun anotherGenerationIsSearchedFromItsStart() {
        val pending = listOf(out("/clear", after = 40, generation = 1))
        val cleared = view(Item.User("/clear", null), generation = 2)
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, cleared, quietSince = null, now = sent))
    }

    /** Only the open chat can be compared; any other waits. */
    @Test
    fun withoutItsChatAMessageWaits() {
        val pending = listOf(out("hello"))
        assertEquals(pending, Outbox.settle(pending, null, quietSince = null, now = sent + 600_000))
    }

    /** A terminal echo is not an acknowledgement in the native conversation. */
    @Test
    fun aScreenDoesNotAcknowledgeASentMessage() {
        val pending = listOf(out("hello"))
        val screen = view(Item.Screen("you said: hello"), generation = 0, source = "screen")
        assertEquals(pending, Outbox.settle(pending, screen, quietSince = null, now = sent))
    }

    @Test
    fun aNativeTranscriptAcknowledgesTheMessageAfterAScreenFallback() {
        val pending = listOf(out("first prompt", after = 0, generation = 0))
        val screen = view(Item.Screen("first prompt\\nWorking"), generation = 0, source = "screen")
        assertEquals(pending, Outbox.settle(pending, screen, null, sent + 60_000))
        val logged = view(Item.User("first prompt", null), Item.Assistant("reply", null), generation = 1)
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, logged, null, sent + 60_000))

        // A screenshot sent from the chat of a session just started, whose
        // conversation the deck already names: Claude's new log is another
        // generation, so only its time tells this message from an earlier
        // equal one, and the phone's clock ran ahead of the desktop's. The
        // copy stayed below the logged message (owner's screenshot,
        // 2026-11-12: "also mesaages with screenshots doublesend, fix that too").
        val shot = "/u/249/Screenshot_20261112-174037_Cepheus.jpg"
        val words = "make it so errors like this automatically go away when devices reconnect"
        val phone = out(words, after = 0, generation = 0, paths = listOf(shot), delivered = false)
            .copy(clientId = "this-send", conversation = "conversation-1")
        val passedOn = sent - 2_000
        val submitted = Outbox.acknowledged(phone, "claude", sent + 400, "submitted", hostAt = passedOn, paths = listOf(shot))
        val firstLog = view(Item.User("$words\n\n$shot", passedOn + 300), Item.Assistant("reply", null), generation = 1)
        assertEquals(emptyList<Outgoing>(), Outbox.settle(listOf(submitted), firstLog, null, sent + 1_000))
        // An equal message logged before the gateway passed this one on is another.
        val earlier = view(Item.User("$words\n\n$shot", passedOn - 60_000), generation = 1)
        assertEquals(listOf(submitted), Outbox.settle(listOf(submitted), earlier, null, sent + 1_000))
    }

    @Test
    fun anUnloggedMessageExpiresOnlyAfterTheSessionGoesQuiet() {
        val pending = listOf(out("/help", after = 0))
        val screen = view(Item.Screen("help: choose a command"), source = "screen")
        assertEquals(pending, Outbox.settle(pending, screen, sent, sent + Outbox.SETTLE_MS - 1))
        assertEquals(emptyList<Outgoing>(), Outbox.settle(pending, screen, sent, sent + Outbox.SETTLE_MS))
    }

    @Test
    fun theChatThinksWhileWorkingStartingOrSendingButStopsWhenQuiet() {
        assertTrue(Outbox.thinking(Status.WORKING, alive = true, pending = emptyList()))
        // Starting is the top bar's to say, until a first message waits on it.
        assertFalse(Outbox.thinking(Status.STARTING, alive = true, pending = emptyList()))
        assertTrue(Outbox.thinking(Status.STARTING, alive = true, pending = listOf(out("hello"))))
        assertFalse(Outbox.thinking(Status.FINISHED, alive = true, pending = listOf(out("hello"))))
        assertTrue(Outbox.thinking(Status.IDLE, alive = true, pending = listOf(out("hello", delivered = false))))
        assertFalse(Outbox.thinking(Status.FINISHED, alive = true, pending = emptyList()))
        assertFalse(Outbox.thinking(Status.IDLE, alive = true, pending = emptyList()))
        // A question shows its card instead, and an ended session does nothing.
        assertFalse(Outbox.thinking(Status.NEEDS_YOU, alive = true, pending = listOf(out("hello"))))
        assertFalse(Outbox.thinking(Status.WORKING, alive = false, pending = emptyList()))
    }

    @Test
    fun deliveredUnloggedMessagesDoNotMakeIdleOrFinishedSessionsThink() {
        for (status in listOf(Status.IDLE, Status.FINISHED)) {
            for (message in listOf("/help", "/model", "hello")) {
                assertFalse("$status with $message", Outbox.thinking(status, alive = true, pending = listOf(out(message))))
            }
        }
        assertFalse(Outbox.thinking(Status.FINISHED, alive = true, pending = listOf(out("hello", delivered = false))))
    }

    @Test
    fun aSessionWaitingOnBackgroundWorkSaysWhatItWaitsOnInsteadOfThinking() {

        assertEquals("Waiting on a shell", Outbox.workingLabel(Status.WORKING, "waiting on a shell"))
        assertEquals("Waiting on 2 background tasks", Outbox.workingLabel(Status.WORKING, " waiting on 2 background tasks "))
        assertEquals("Thinking…", Outbox.workingLabel(Status.WORKING, ""))
        // A first message to a starting or idle session is the agent's to think about.
        assertEquals("Thinking…", Outbox.workingLabel(Status.STARTING, ""))
        assertEquals("Thinking…", Outbox.workingLabel(Status.IDLE, "waiting on a shell"))
    }






    @Test
    fun theWorkingTimeCountsFromTheNewestMessageNotTheFirstTurn() {
        val workingSince = 26_460L // 07:21
        val later = view(Item.User("first", 26_460_000), Item.Assistant("ok", null), Item.User("and now this", 36_000_000))
        assertEquals(36_000L, Outbox.workingSince(workingSince, later.items, emptyList()))
        // One the log does not show yet counts as soon as it is sent.
        val sending = out("hello").copy(createdAt = 50_000_000)
        assertEquals(50_000L, Outbox.workingSince(workingSince, later.items, listOf(sending)))
        // A turn that started after the newest message counts from its start.
        assertEquals(40_000L, Outbox.workingSince(40_000, later.items, emptyList()))
        assertEquals(40_000L, Outbox.workingSince(40_000, emptyList(), emptyList()))
    }
}
