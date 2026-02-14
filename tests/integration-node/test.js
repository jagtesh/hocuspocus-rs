const Y = require('yjs')
const { HocuspocusProvider } = require('@hocuspocus/provider')
const ws = require('ws')

const roomName = 'test-room-' + Math.random().toString(36).substring(7)
const serverUrl = 'ws://127.0.0.1:1234/sync'

console.log(`Connecting to ${serverUrl} room ${roomName}`)

const ydoc = new Y.Doc()
const provider = new HocuspocusProvider({
  url: serverUrl,
  name: roomName,
  document: ydoc,
  WebSocketPolyfill: ws,
  onStatus: event => {
    console.log('Status:', event.status)
  },
  onSynced: () => {
    console.log('Synced successfully')
  },
  onClose: () => {
    console.log('Connection closed')
  },
  onDisconnect: () => {
    console.log('Disconnected')
  }
})

const ytext = ydoc.getText('content')

// Wait for sync
let syncResolved = false
provider.on('synced', () => {
  if (!syncResolved) {
    syncResolved = true
    console.log('Connected and synced successfully via Hocuspocus protocol')
    
    // Perform an edit
    ytext.insert(0, 'Hello from Hocuspocus Node.js client!')
    
    // Wait for a bit to ensure broadcast/roundtrip (though here it's local)
    setTimeout(() => {
      console.log('Final content:', ytext.toString())
      if (ytext.toString().includes('Hello from Hocuspocus Node.js client!')) {
        console.log('Integration test PASSED')
        process.exit(0)
      } else {
        console.error('Integration test FAILED: content mismatch')
        process.exit(1)
      }
    }, 1000)
  }
})

setTimeout(() => {
  if (!syncResolved) {
    console.error('Failed to sync within timeout')
    process.exit(1)
  }
}, 5000)
