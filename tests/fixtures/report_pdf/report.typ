#import "@embarch/report:0.1.0": minmax-plot, m

= Fixture report

The sample rate was #m("rate", path("measurements.json")).

#minmax-plot(path("capture.csv"), x: "t_s", y: "value", bins: 50, xlabel: [time], ylabel: [signal])
