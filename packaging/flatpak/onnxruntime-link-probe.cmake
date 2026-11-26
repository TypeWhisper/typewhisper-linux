# The Parakeet add-on loads this file into ONNX Runtime's CMake project through
# CMAKE_PROJECT_onnxruntime_INCLUDE.
#
# A static ONNX Runtime build declares `onnxruntime` only as an interface target:
# nothing in the default build links it, so libraries that only that target needs
# (re2, for example) are never built. This adds an empty program that links it.
# Building it makes ninja build every library ONNX Runtime needs, and its link
# command lists them, which the manifest joins into one libonnxruntime.a.
cmake_language(DEFER DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}" CALL _typewhisper_onnxruntime_link_probe)

function(_typewhisper_onnxruntime_link_probe)
  set(source "${CMAKE_CURRENT_BINARY_DIR}/onnxruntime_link_probe.c")
  file(WRITE "${source}" "int main(void) { return 0; }\n")
  add_executable(onnxruntime_link_probe "${source}")
  target_link_libraries(onnxruntime_link_probe PRIVATE onnxruntime)
endfunction()
