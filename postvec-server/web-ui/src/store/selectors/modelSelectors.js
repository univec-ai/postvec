export const getModels = (state) => {
  const alias = state.modelhub?.prefs?.active_remote_alias || 'local'
  const models = state.modelhub?.filesystem?.[alias]?.models || []
  return models instanceof Array ? models : []
}

export const getFilteredModels = (state) => {
  const models = getModels(state)
  const filter = state.prefs.filter || ''
  const regex = new RegExp(filter, 'i')
  return models.filter((model) => filter.length === 0 || regex.test(model.name))
}

export const getFilteredEnabledModels = (state) => {
  return getFilteredModels(state).filter((model) => model.configuration?.enabled)
}

export const getCurrentModel = (state) => {
  const models = getFilteredEnabledModels(state)
  const currentName = state.prefs.current_model_name
  const filtered = models.filter((model) => model.name === currentName)
  if (filtered.length > 0) {
    return filtered[0]
  }
  return models.length > 0 ? models[0] : false
}

export const getCurrentModelInputs = (state) => {
  const currentModel = getCurrentModel(state)
  if (!currentModel) {
    return []
  }
  const executorInputs = currentModel.configuration?.executor?.inputs || []
  return executorInputs
    .map((input) => input.json_key || '')
    .filter((key) => key.length > 0)
}

export const getCurrentQueryString = (state) => {
  const currentModel = getCurrentModel(state)
  if (!currentModel) {
    return ''
  }
  const modelCache = state.prefs.query_cache.models[currentModel.name] || {}
  if (modelCache.text) {
    return modelCache.text
  }
  const cachedModelInputs = getCurrentModelInputs(state).reduce((acc, curr) => {
    acc[curr] = ''
    return acc
  }, {})
  return JSON.stringify(cachedModelInputs, null, 2)
}

export const getCurrentModelQueryLoading = (state) => {
  const currentModel = getCurrentModel(state)
  if (!currentModel) return false
  return state.prefs.query_cache.models[currentModel.name]?.loading || false
}

export const getCurrentModelQueryPerformed = (state) => {
  const currentModel = getCurrentModel(state)
  if (!currentModel) return false
  return state.prefs.query_cache.models[currentModel.name]?.performed || false
}

export const getCurrentModelQueryResponses = (state) => {
  const currentModel = getCurrentModel(state)
  if (!currentModel) return []
  return state.prefs.query_cache.models[currentModel.name]?.responses || []
}
