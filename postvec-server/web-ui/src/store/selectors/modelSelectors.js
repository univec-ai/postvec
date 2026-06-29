export const getFilteredModels = (state) => {
  const filter = (state.prefs.filter || '').toLowerCase()
  return state.models.filter(
    (model) => model.configuration?.enabled !== false && model.name.toLowerCase().includes(filter),
  )
}

export const getCurrentModel = (state) => {
  const models = getFilteredModels(state)
  return models.find((model) => model.name === state.prefs.current_model_name) || models[0] || null
}

export const isEmbedModel = (model) =>
  (model?.configuration?.params?.model_type || 'embed').toLowerCase() !== 'convert'

const currentCache = (state) => {
  const model = getCurrentModel(state)
  return model ? state.prefs.query_cache.models[model.name] || {} : {}
}

// OpenAI only serves embed models; a converter is always native.
export const getCurrentContract = (state) =>
  isEmbedModel(getCurrentModel(state)) ? currentCache(state).contract || 'native' : 'native'

export const getCurrentModelInputs = (state) =>
  (getCurrentModel(state)?.configuration?.executor?.inputs || [])
    .map((input) => input.json_key)
    .filter(Boolean)

// Editor text: what the user last typed for this model and contract, else a
// template with every executor input (arrays for the batch inputs).
export const getCurrentQueryString = (state) => {
  const model = getCurrentModel(state)
  if (!model) return ''
  const contract = getCurrentContract(state)
  const cached = currentCache(state).text?.[contract]
  if (cached !== undefined) return cached
  if (contract === 'openai') {
    return JSON.stringify({ model: model.name, input: [''] }, null, 2)
  }
  const inputs = getCurrentModelInputs(state)
  const keys = inputs.length > 0 ? inputs : [isEmbedModel(model) ? 'texts' : 'embeddings']
  const template = Object.fromEntries(
    keys.map((key) => [key, key === 'texts' || key === 'embeddings' ? [] : '']),
  )
  return JSON.stringify(template, null, 2)
}

export const getCurrentModelQueryLoading = (state) => !!currentCache(state).loading

export const getCurrentModelQueryResponses = (state) => currentCache(state).responses || []
