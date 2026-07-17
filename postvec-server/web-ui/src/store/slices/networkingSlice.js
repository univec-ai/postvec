// The node that served the page is the endpoint. `VITE_API_ENDPOINT` at
// build time overrides it for `npm run dev` against a TLS node; the dev
// server also proxies /config and /api there. Never persisted: the same
// build may be served by another node tomorrow.
const createNetworkingSlice = () => ({
  api_endpoint: import.meta.env.VITE_API_ENDPOINT || window.location.origin,
  cluster: [],
  system: null,
  server: null,
  loading: false,
  error: null,
})

export default createNetworkingSlice
