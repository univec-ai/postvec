import React from 'react'
import CodeMirror from '@uiw/react-codemirror'
import { json } from '@codemirror/lang-json'
import { EditorView } from '@codemirror/view'

const mono = "'Source Code Pro', 'Menlo', 'Monaco', 'Consolas', monospace"

const theme = EditorView.theme({
  '&': { fontFamily: mono, fontSize: '13px', backgroundColor: '#faf8f4' },
  '.cm-content': { fontFamily: mono },
})

// `height` fixes the box; without it the editor grows to its content.
const CodeEditor = ({ height, value, onChange, readOnly = false }) => (
  <CodeMirror
    value={value}
    height={height ? `${height}px` : undefined}
    minHeight={height ? undefined : '120px'}
    extensions={[json(), theme, EditorView.lineWrapping]}
    onChange={onChange}
    readOnly={readOnly}
    basicSetup={{ foldGutter: true, highlightActiveLine: !readOnly }}
  />
)

export default CodeEditor
