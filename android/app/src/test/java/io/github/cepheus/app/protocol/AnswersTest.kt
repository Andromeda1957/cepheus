package io.github.cepheus.app.protocol

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** How the phone answers the prompts cepheus/src/prompt.rs reads. */
class AnswersTest {

    private val several = Protocol.parse(
        """{"t":"prompt","id":28,"inputs":4,"fp":"c0ffee","prompt":{"kind":"question","question":"Which match?",""" +
            """"options":[{"label":"Connection flicker","checked":false},{"label":"Messages feel unsent","checked":true},""" +
            """{"label":"Looks thin and flat","checked":false},{"label":"Type something","checked":false},""" +
            """{"label":"Chat about this"}],"selected":0,"free_text":3,"multi":true,"submit":"Submit"}}""",
    ).let { (it as ServerMessage.PromptUpdate).prompt!! }

    private val single = Prompt(
        "question",
        "Which one?",
        listOf(PromptOption("Red", ""), PromptOption("Type something.", ""), PromptOption("Chat about this", "")),
        0,
        1,
        false,
    )

    @Test
    fun aSeveralChoiceCardStartsWithTheDesksTicks() {
        assertEquals(setOf(1), Answers.ticked(several))
        assertTrue(Answers.isBox(several, 0))
        assertFalse("Chat about this answers alone", Answers.isBox(several, 4))
        assertFalse("a single choice has no boxes", Answers.isBox(single, 0))
    }

    @Test
    fun theBoxesGoTogetherWithTheFreeTextOnlyWhenItsBoxIsPicked() {
        assertEquals(Answer(1, null, listOf(1, 2)), Answers.several(several, setOf(2, 1), "ignored"))
        assertEquals(Answer(0, "mango", listOf(0, 3)), Answers.several(several, setOf(3, 0), " mango "))
        assertNull("nothing picked", Answers.several(several, emptySet(), null))
        assertNull("the free-text box needs text", Answers.several(several, setOf(3), " "))
        assertNull("Chat about this is no box", Answers.several(several, setOf(4), null))
        assertNull("no Submit row read", Answers.several(several.copy(submit = null), setOf(1), null))
    }

    @Test
    fun textTypedInTheMessageBoxAnswersThroughTheFreeTextOption() {
        assertEquals(Answer(1, "green", emptyList()), Answers.typed(single, emptySet(), " green "))
        assertEquals(Answer(1, "mango", listOf(1, 3)), Answers.typed(several, setOf(1), "mango"))
        assertNull("nothing typed", Answers.typed(single, emptySet(), "  "))
        val approval = single.copy(kind = "permission", freeText = null)
        assertNull("an approval takes a tap", Answers.typed(approval, emptySet(), "yes"))
    }
}
