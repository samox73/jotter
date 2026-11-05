# ---
# jupyter:
#   jupytext:
#     text_representation:
#       extension: .py
#       format_name: percent
#       format_version: '1.3'
#       jupytext_version: 1.19.6
#   kernelspec:
#     display_name: phy
#     language: python
#     name: phy
# ---

# %% [markdown]
# # jOtter test notebook
#
# Both latex in-line math $ a^2 + b^2 $ and display math work:
#
# $$ \int_0^\infty d\mathbf{x} e^{-x} = 1 $$
#
# Bullet list:
#
# - also
# - work
#
# Inline code `if it_works { println!("noice"); }` and code blocks too
#
# ```rust
# if it_works {
#   println!("even noicer!");
# }
# ```

# %%
import numpy as np
import matplotlib.pyplot as plt

# %%
print("hello from stdout")
print("line two")

# %%
tau = np.linspace(0, np.pi, 5)  # imaginary-time grid, eV^-1
tau

# %%
plt.plot(tau, np.exp(-tau))
plt.xlabel(r"$\tau$ (eV$^{-1}$)")
plt.show()
