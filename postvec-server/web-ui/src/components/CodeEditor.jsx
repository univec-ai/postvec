import React from 'react'
import CodeMirror from '@uiw/react-codemirror'
import { json } from '@codemirror/lang-json'
import { EditorView, keymap } from '@codemirror/view'
import { Prec } from '@codemirror/state'

const theme = EditorView.theme({
  '&': { backgroundColor: 'var(--code-bg)', height: '100%' },
})

const CodeEditor = ({ value, onChange, onSubmit, readOnly = false }) => {
  const extensions = [json(), theme, EditorView.lineWrapping]
  if (onSubmit) {
    extensions.push(Prec.highest(keymap.of([{ key: 'Mod-Enter', run: () => (onSubmit(), true) }])))
  }
  return (
    <CodeMirror
      value={value}
      height="100%"
      extensions={extensions}
      onChange={onChange}
      readOnly={readOnly}
      basicSetup={{ foldGutter: true, highlightActiveLine: !readOnly }}
    />
  )
}

export default CodeEditor
