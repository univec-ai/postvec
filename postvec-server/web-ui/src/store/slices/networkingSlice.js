// Same origin as the page: the node that served it, or the Vite proxy
// (`npm run dev` forwards /config and /api). Not persisted — the same
// build may be served by another node tomorrow.
const createNetworkingSlice = () => ({
  api_endpoint:
    typeof window !== 'undefined' && window.location.protocol.startsWith('http')
      ? window.location.origin
      : 'http://127.0.0.1:22222',
  cluster: [],
  system: null,
  server: null,
  loading: false,
  error: null,
})

export default createNetworkingSlice
