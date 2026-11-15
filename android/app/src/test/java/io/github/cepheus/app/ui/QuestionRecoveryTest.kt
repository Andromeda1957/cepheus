package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.AsyncQuestion
import io.github.cepheus.app.protocol.Prompt
import io.github.cepheus.app.protocol.WaitingPrompt
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test







class QuestionRecoveryTest {
    private val old = AsyncQuestion("old:0", "Old?", listOf("Yes", "No"))
    private val other = AsyncQuestion("old:1", "Other?", listOf("A", "B"))
    private val fresh = AsyncQuestion("new:0", "New?", listOf("Yes", "No"))
    private val prompt = WaitingPrompt(1, 3, "fp-1", Prompt("async question", "Old?", emptyList(), 0, null, false))

    @Test fun aWaitingPromptCanBeSetAsideForTheComposer() {
        val shown = QuestionRecovery.shown(emptyList(), null, prompt)
        assertTrue("an open prompt takes the composer", QuestionRecovery.answering(true, false, QuestionRecovery.setAside(shown, emptySet()), false))
        val aside = QuestionRecovery.setAside(shown, shown)
        assertTrue(aside)
        assertFalse("a prompt set aside gives the composer back", QuestionRecovery.answering(true, false, aside, false))
        // Read again with another input count: still the same question.
        val reread = QuestionRecovery.shown(emptyList(), null, prompt.copy(inputs = 4))
        assertTrue(QuestionRecovery.setAside(reread, shown))
        // Another question replaces it on the screen.
        val replaced = QuestionRecovery.shown(emptyList(), null, prompt.copy(fp = "fp-2"))
        assertFalse(QuestionRecovery.setAside(replaced, shown))
    }

    @Test fun cardsSetAsideStayAsideWhileTheirListChanges() {
        val shown = QuestionRecovery.shown(listOf(old, other), old, null)
        assertEquals(setOf("card:old:0", "card:old:1"), shown)
        assertTrue(QuestionRecovery.answering(false, true, QuestionRecovery.setAside(shown, emptySet()), false))
        // The deck withdrew the refused card; the one left was set aside with it.
        val withdrawn = QuestionRecovery.shown(listOf(other), other, null)
        assertTrue(QuestionRecovery.setAside(withdrawn, shown))
        assertFalse(QuestionRecovery.answering(false, true, QuestionRecovery.setAside(withdrawn, shown), false))
        // A question asked afterwards opens again.
        val asked = QuestionRecovery.shown(listOf(other, fresh), other, null)
        assertFalse(QuestionRecovery.setAside(asked, shown))
    }

    @Test fun aWithdrawnCardDoesNotHandTheComposerToItsScreenPrompt() {
        // The card hides the screen's prompt for the same question while it shows.
        val shown = QuestionRecovery.shown(listOf(old), old, null)
        val aside = QuestionRecovery.staleAside(shown, prompt)
        // The deck withdrew the refused card, and the prompt shows in its place.
        val left = QuestionRecovery.shown(emptyList(), null, prompt)
        assertTrue(QuestionRecovery.setAside(left, aside))
        assertFalse(QuestionRecovery.answering(true, false, QuestionRecovery.setAside(left, aside), false))
        // Another question on the screen takes the composer.
        assertFalse(QuestionRecovery.setAside(QuestionRecovery.shown(emptyList(), null, prompt.copy(fp = "fp-2")), aside))
    }

    @Test fun nothingShownIsNeverSetAside() {
        assertFalse(QuestionRecovery.setAside(emptySet(), setOf("card:old:0")))
        assertFalse(QuestionRecovery.answering(false, false, false, false))
    }

    @Test fun aSubmittedCardLeavesTheComposerForMessages() {
        assertFalse(QuestionRecovery.answering(false, true, false, true))
    }
}
