package io.github.cepheus.app.ui

import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.TextFieldValue
import io.github.cepheus.app.protocol.CommandEntry
import org.junit.Assert.*
import org.junit.Test

class CommandsTest {
    private val model = CommandEntry("/model", "/model", "Choose a model", "command")
    private val skill = CommandEntry("$" + "review", "$" + "review", "Review changes", "skill")

    @Test fun slashIncludesEverySkillAndCommandWithoutATopResultsCutoff() {
        val entries = (0 until 130).map { CommandEntry("/skill-$it", "/skill-$it", "Skill", "skill") } + model
        assertEquals(131, commandMatches(entries, commandQuery(TextFieldValue("/", TextRange(1)))!!).size)
        val dollar = commandQuery(TextFieldValue("$", TextRange(1)))!!
        assertEquals(listOf(skill), commandMatches(listOf(model, skill), dollar))
        val search = commandQuery(TextFieldValue("/CHANGE", TextRange(7)))!!
        assertEquals(listOf(skill), commandMatches(listOf(model, skill), search))
    }

    @Test fun pickingOnlyReplacesTheCurrentTokenAndKeepsArgumentsAndOtherText() {
        val value = TextFieldValue("Please /rev today", TextRange(11))
        val inserted = insertCommand(value, commandQuery(value)!!, skill)
        assertEquals("Please $" + "review today", inserted.text)
        assertEquals(TextRange(15), inserted.selection)
        val bare = TextFieldValue("/mo", TextRange(3))
        assertEquals("/model ", insertCommand(bare, commandQuery(bare)!!, model).text)
        assertNull(commandQuery(TextFieldValue("/model ", TextRange(7))))
        assertNull(commandQuery(TextFieldValue("/tmp/file", TextRange(9))))
        assertNull(commandQuery(TextFieldValue("ordinary text", TextRange(13))))
        assertNull(commandQuery(TextFieldValue("/mod", TextRange(0, 4))))
    }
}
