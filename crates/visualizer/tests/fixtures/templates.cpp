// Real libstdc++ types, plus Eigen and units shapes as the firmware has them.
#include <array>
#include <cstdint>
#include <optional>
#include <ratio>
#include <span>
#include <string_view>
#include <variant>

namespace units {
template <class Meter = std::ratio<0>, class Kilogram = std::ratio<0>, class Second = std::ratio<0>,
          class Radian = std::ratio<0>, class Ampere = std::ratio<0>, class Kelvin = std::ratio<0>,
          class Mole = std::ratio<0>, class Candela = std::ratio<0>, class Byte = std::ratio<0>>
struct base_unit {};
template <class Conversion, class Base, class Pi = std::ratio<0>, class Translation = std::ratio<0>>
struct unit {};
template <class T>
struct linear_scale {
    T m_value;
};
template <class U, class T>
struct unit_t : linear_scale<T> {};
}  // namespace units

namespace Eigen {
template <class T, int N, int A>
struct plain_array {
    T array[N];
};
template <class T, int N, int R, int C, int O>
struct DenseStorage {
    plain_array<T, N, 0> m_data;
};
template <class D>
struct MatrixBase {};
template <class T, int R, int C, int O, int MR, int MC>
struct Matrix : MatrixBase<Matrix<T, R, C, O, MR, MC>> {
    DenseStorage<T, R * C, R, C, O> m_storage;
};
template <class T, int O>
struct Quaternion {
    Matrix<T, 4, 1, 0, 4, 1> m_coeffs;
};
}  // namespace Eigen

namespace etl {
template <class T>
struct ivector {
    T* p_buffer;
    T* p_end;
};
template <class T, int N>
struct vector : ivector<T> {
    T storage[N];
};
}  // namespace etl

using Radian = units::unit_t<
    units::unit<std::ratio<1>, units::base_unit<std::ratio<0>, std::ratio<0>, std::ratio<0>, std::ratio<1>>>,
    float>;
// kg·m²/s²
using Torque = units::unit_t<
    units::unit<std::ratio<1>, units::base_unit<std::ratio<2>, std::ratio<1>, std::ratio<-2>>>, float>;
using Millimeter = units::unit_t<
    units::unit<std::ratio<1, 1000>, units::unit<std::ratio<1>, units::base_unit<std::ratio<1>>>>, float>;

[[gnu::used]] Radian g_unit = {{1.5f}};
[[gnu::used]] Torque g_torque = {{2.0f}};
[[gnu::used]] Millimeter g_mm = {{3.0f}};
[[gnu::used]] Eigen::Matrix<float, 3, 1, 0, 3, 1> g_vector = {{}, {{{1.0f, 2.0f, 3.0f}}}};
// Column-major: columns are (1,2) (3,4) (5,6).
[[gnu::used]] Eigen::Matrix<float, 2, 3, 0, 2, 3> g_matrix = {{}, {{{1, 2, 3, 4, 5, 6}}}};
// Row-major: rows are (1,2,3) (4,5,6).
[[gnu::used]] Eigen::Matrix<float, 2, 3, 1, 2, 3> g_row_major = {{}, {{{1, 2, 3, 4, 5, 6}}}};
[[gnu::used]] Eigen::Quaternion<float, 0> g_quat = {{{}, {{{0.1f, 0.2f, 0.3f, 0.9f}}}}};
[[gnu::used]] std::array<int16_t, 3> g_array = {7, 8, 9};
[[gnu::used]] std::optional<float> g_some = 2.5f;
[[gnu::used]] std::optional<float> g_none;
[[gnu::used]] std::variant<int, float> g_variant = 1.25f;
[[gnu::used]] int g_span_data[3] = {11, 22, 33};
[[gnu::used]] std::span<int> g_span{g_span_data};
[[gnu::used]] std::string_view g_view = "hello";
[[gnu::used]] etl::vector<int, 4> g_etl = {{g_etl.storage, g_etl.storage + 2}, {5, 6}};
struct Plain {
    int a;
};
[[gnu::used]] Plain g_plain = {3};

extern "C" [[gnu::used]] void entry() {
    while (true) {
    }
}
