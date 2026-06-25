import React, { useRef } from 'react'
import CodeMirror from '@uiw/react-codemirror'
import { javascript } from '@codemirror/lang-javascript'
import { json } from '@codemirror/lang-json'
import { EditorView } from '@codemirror/view'

const CodeEditor = ({ height, maxHeight, value, onChange, language }) => {
  const editorRef = useRef(null)
  const handleChange = (newValue) => onChange && onChange(newValue)
  const editorHeight =
    height === 'auto'
      ? `${maxHeight}px`
      : typeof height === 'number'
        ? `${height}px`
        : height

  let langExtension
  switch (language) {
    case 'json':
      langExtension = json()
      break
    case 'javascript':
    default:
      langExtension = javascript({ jsx: true })
      break
  }

  const fontTheme = EditorView.theme({
    '&': {
      fontFamily: "'Source Code Pro', 'Menlo', 'Monaco', 'Consolas', monospace",
      fontSize: '13px',
      backgroundColor: '#faf8f4',
    },
    '.cm-content': {
      fontFamily: "'Source Code Pro', 'Menlo', 'Monaco', 'Consolas', monospace",
      fontSize: '13px',
      whiteSpace: 'pre-wrap',
      wordWrap: 'break-word',
    },
  })

  return (
    <CodeMirror
      ref={editorRef}
      value={value}
      height={editorHeight}
      maxHeight={`${maxHeight}px`}
      extensions={[langExtension, fontTheme, EditorView.lineWrapping]}
      onChange={handleChange}
      basicSetup={{
        lineNumbers: true,
        foldGutter: true,
        bracketMatching: true,
        highlightActiveLine: true,
        highlightSelectionMatches: true,
      }}
    />
  )
}

export default CodeEditor
